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
const FILES = ["src/api/users.ts", "src/repo/users.ts", "src/middleware/auth.ts", "tests/users.test.ts", "src/web/useKeyboardInset.ts", "README.md", "src/config.ts"];
const BASH = [
  { command: "npm test", out: "✓ users list (12 ms)\n✓ users paging (8 ms)\n\nTests: 2 passed", ok: true },
  { command: "npm test -- users", out: "✗ users paging\n  expected 20, got 0\n\nTests: 1 failed, 1 passed", ok: false },
  { command: "git status --short", out: " M src/api/users.ts\n?? tests/users.test.ts", ok: true },
  { command: "ls src/api", out: "health.ts\nusers.ts", ok: true },
];
const DANGER = [{ command: "git push --force origin main", why: "強制推送會覆蓋遠端歷史", rule: "git-force-push" }, { command: "rm -rf dist build", why: "遞迴刪除", rule: "rm-rf" }];
const AGENT_TYPES = ["Explore", "general-purpose", "Plan"];
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
function createWorld(n) {
  const r = rng(20261016);
  const pick = (a) => a[Math.floor(r() * a.length)];
  const now = Date.now();
  const sessions = Array.from({ length: n }, (_, i) => ({
    id: uuid(r),
    host: "local",
    cwd: "/home/me/my-api",
    branch: i % 3 === 0 ? "develop" : `feature/t${i}`,
    model: i % 4 === 0 ? "opus" : "sonnet",
    firstPrompt: PROMPTS[i % PROMPTS.length],
    lastPrompt: PROMPTS[i % PROMPTS.length],
    startedAt: now - (n - i) * 60_000,
    endedAt: null,
    status: "run",
    statusSince: now,
    doing: null,
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
      tool = "Edit"; input = { file_path: pick(FILES), old_string: "const page = 1;", new_string: "const page = Number(req.query.page ?? 1);" };
      t.derived.push({ type: "file.edit", payload: { path: input.file_path, hunk: { start: 41, oldLines: 3, newLines: 4, lines: [" export async function listUsers(req, res) {", "-  const page = 1;", "+  const page = Number(req.query.page ?? 1);", "+  const size = Math.min(100, Number(req.query.size ?? 20));", "   const rows = await repo.list({ page, size });"] }, hunkSource: "pre" } });
      kpi.edits++;
    } else if (y < 0.56) {
      tool = "Write"; input = { file_path: `src/new/${id.slice(-4)}.ts` };
      t.derived.push({ type: "file.write", payload: { path: input.file_path, isNew: true, content: "export const x = 1;\nexport const y = 2;\n", truncated: false } });
      kpi.writes++;
    } else if (y < 0.62 && [...agents.values()].filter((a) => a.session === s.id).length < 4) {
      tool = "Task"; input = { description: "查 users 相關的呼叫點", subagent_type: pick(AGENT_TYPES) };
      const agentId = `a${(++agentSeq).toString(16).padStart(16, "0")}`;
      const name = `查呼叫點 ${agentSeq}`;
      const a = { id: `${s.id}/${agentId}`, session: s.id, agentId, agentType: input.subagent_type, name, task: input.description, status: "run", current: null, toolCount: 0, parentToolUseId: id, spawnedAt: Date.now(), finishedAt: null };
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

  function statusStep(s, events, rows) {
    for (const [id, a] of attention) if (a.session === s.id) { attention.delete(id); rows.attention.push({ id, deleted: true }); }
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
      const a = { id: `${s.id}/${s.status}/toolu_${frame}`, session: s.id, kind: s.status, ref: `toolu_${frame}`, reason: ATTENTION_REASON[s.status], raisedAt: Date.now() };
      attention.set(a.id, a);
      rows.attention.push(a);
    }
    if (s.status === "danger") changed.dangers = ++s.dangers;
    if (s.status === "fail") changed.failures = ++s.failures;
    rows.sessions.push(changed);
  }

  function finishAgents(s, rows) {
    for (const a of agents.values()) {
      if (a.session !== s.id || a.agentId === "main") continue;
      if (a.agentId === "bg") {
        if (r() < 0.3) { agents.delete(a.id); rows.agents.push({ id: a.id, deleted: true }); }
      } else if (a.status === "run" && r() < 0.08) {
        Object.assign(a, { status: "done", current: null, finishedAt: Date.now() });
        rows.agents.push({ id: a.id, status: "done", current: null, finishedAt: a.finishedAt });
      }
    }
    if (!agents.has(`${s.id}/bg`) && r() < 0.03) {
      const b = { id: `${s.id}/bg`, session: s.id, agentId: "bg", agentType: null, name: "背景", task: null, status: "run", current: null, toolCount: 0, parentToolUseId: null, spawnedAt: Date.now(), finishedAt: null };
      agents.set(b.id, b);
      rows.agents.push(b);
    }
    s.agentsLive = [...agents.values()].filter((a) => a.session === s.id && a.agentId !== "main" && a.agentId !== "bg" && a.status === "run").length;
  }

  return {
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
    more(session, beforePos, limit) {
      const mine = glog.filter((e) => e.session === session && e.pos != null && e.pos < beforePos);
      return { t: "more", session, events: mine.slice(-limit).flatMap(expand) };
    },
    step({ minEvents = 0 } = {}) {
      frame++;
      const events = [];
      const rows = { sessions: [], agents: [], attention: [] };
      const ticks = minEvents ? 10_000 : 1 + Math.floor(r() * 3);
      for (let k = 0; k < ticks && (!minEvents || events.length < minEvents); k++) {
        const s = pick(sessions);
        if (r() < (minEvents ? 0.02 : 0.08)) statusStep(s, events, rows);
        if (s.status !== "run" && r() < 0.15) { // 停下來的 session 過一陣子繼續跑
          s.status = "run";
          s.statusSince = Date.now();
          rows.sessions.push({ id: s.id, status: "run", statusSince: s.statusSince });
          for (const [id, a] of attention) if (a.session === s.id) { attention.delete(id); rows.attention.push({ id, deleted: true }); }
        }
        tick(s, events, rows);
        finishAgents(s, rows);
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

export function installMock(n, { bench = false, prefill = false } = {}) {
  const world = createWorld(n);
  if (bench || prefill) world.prefill(220);
  const live = new Set();
  let downUntil = 0;

  class MockSocket {
    constructor() {
      this.readyState = 0;
      this.timer = null;
      live.add(this);
      setTimeout(() => {
        if (Date.now() < downUntil) return this.#end(); // collector「還沒起來」
        this.readyState = 1;
        this.onopen?.({});
      }, 30);
    }
    send(text) {
      const msg = JSON.parse(text);
      if (msg.op === "hello") this.#emit({ t: "welcome", protocol: 1, host: "local", version: "0.0.0-mock", remote: false, thinking: "full", scope: "operate" });
      if (msg.op === "subscribe" && msg.channel === "*") {
        this.#emit(world.snapshot());
        this.timer = bench
          ? setInterval(() => this.#emit(world.step({ minEvents: 20 })), 50)
          : setInterval(() => this.#emit(world.step()), 400);
      }
      if (msg.op === "more") setTimeout(() => this.#emit(world.more(msg.session, msg.beforePos, msg.limit)), 60);
    }
    close() { this.#end(); }
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
    // 模擬 collector 重啟：所有連線斷掉，之後 ms 毫秒內連不上
    restart(ms = 3000) {
      downUntil = Date.now() + ms;
      for (const s of [...live]) s.close();
    },
  };
}
