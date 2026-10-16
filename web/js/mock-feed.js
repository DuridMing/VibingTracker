// 開發用假 collector：網址加 ?mock=<session 數> 時取代 WebSocket。
// 送的是契約（contracts.md §6）格式的 welcome／snapshot／frame，走跟真連線同一條 client 路徑。
// 只捏我們自己的 Event／rows 格式，不捏 transcript。

const STATUSES = ["run", "run", "run", "wait", "done", "fail", "danger"];
const PROMPTS = [
  "列表 API 加分頁", "修 auth middleware 的 token 過期", "加 health endpoint", "重構 repo 層",
  "補 users 的整合測試", "手機鍵盤遮住輸入框", "升級 express 到 5", "整理 CI cache",
  "把設定改成 toml", "修 flaky 的 e2e", "加 rate limit", "清掉舊的 migration",
  "寫 README 安裝段落", "拆 monorepo 的 web 套件", "改 log 格式", "加 OpenAPI 文件",
];

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

const AGENT_TYPES = ["Explore", "general-purpose", "Plan"];
const ACTIONS = ["read src/api/users.ts", "edit src/api/users.ts", "$ cargo test", "$ git status", "write tests/users.test.ts", "thinking…"];
const ATTENTION_KIND = { wait: "wait", done: "done", fail: "fail", danger: "danger" };
const ATTENTION_REASON = { wait: "要不要同時更新 README？", done: "回合結束，等你確認", fail: "cargo test exit 101（連續兩次）", danger: "git push --force" };

// collector 端的狀態在所有連線之間共用，斷線重連後還是同一批 session。
// 欄位照 contracts.md §6「彙總列」。
function createWorld(n) {
  const r = rng(20261016);
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
  const agents = new Map();
  const attention = new Map();
  for (const s of sessions) {
    agents.set(`${s.id}/main`, { id: `${s.id}/main`, session: s.id, agentId: "main", agentType: null, name: "main", task: null, status: "run", current: null, toolCount: 0, parentToolUseId: null, spawnedAt: s.startedAt, finishedAt: null });
  }
  const kpi = { live: n, attention: 0, sessions: n, agents: 0, edits: 0, writes: 0, commands: 0, failures: 0, dangers: 0, commits: 0, thinkingTokens: 0, costUsd: 0 };
  let frame = 0;
  let agentSeq = 0;

  function recount() {
    kpi.live = sessions.filter((s) => s.status === "run").length;
    kpi.attention = attention.size;
  }

  function stepAgents(s, out) {
    const mine = [...agents.values()].filter((a) => a.session === s.id);
    const x = r();
    if (x < 0.12 && mine.length < 4) {
      // 派出 subagent
      const agentId = `a${(++agentSeq).toString(16).padStart(6, "0")}`;
      const a = { id: `${s.id}/${agentId}`, session: s.id, agentId, agentType: AGENT_TYPES[agentSeq % AGENT_TYPES.length], name: AGENT_TYPES[agentSeq % AGENT_TYPES.length], task: "查 users 相關的呼叫點", status: "run", current: null, toolCount: 0, parentToolUseId: `toolu_${agentId}`, spawnedAt: Date.now(), finishedAt: null };
      agents.set(a.id, a);
      out.push(a);
      kpi.agents++;
    } else if (x < 0.2) {
      // 有 subagent 完成，或背景任務開始／結束
      const sub = mine.find((a) => a.agentId !== "main" && a.agentId !== "bg" && a.status === "run");
      const bg = agents.get(`${s.id}/bg`);
      if (sub) {
        Object.assign(sub, { status: "done", current: null, finishedAt: Date.now() });
        out.push({ id: sub.id, status: "done", current: null, finishedAt: sub.finishedAt });
      } else if (bg) {
        agents.delete(bg.id);
        out.push({ id: bg.id, deleted: true });
      } else {
        const b = { id: `${s.id}/bg`, session: s.id, agentId: "bg", agentType: null, name: "背景", task: null, status: "run", current: null, toolCount: 0, parentToolUseId: null, spawnedAt: Date.now(), finishedAt: null };
        agents.set(b.id, b);
        out.push(b);
      }
    } else {
      const running = mine.filter((a) => a.status === "run" && a.agentId !== "bg");
      const a = running[Math.floor(r() * running.length)];
      if (a) {
        a.current = ACTIONS[Math.floor(r() * ACTIONS.length)];
        a.toolCount++;
        out.push({ id: a.id, current: a.current, toolCount: a.toolCount });
      }
    }
    s.agentsLive = [...agents.values()].filter((a) => a.session === s.id && a.agentId !== "main" && a.agentId !== "bg" && a.status === "run").length;
  }

  return {
    snapshot() {
      return {
        t: "snapshot", ch: "*", frame,
        sessions: sessions.map((s) => ({ ...s })),
        kpi: { ...kpi },
        agents: [...agents.values()].map((a) => ({ ...a })),
        attention: [...attention.values()].map((a) => ({ ...a })),
        changes: [], staged: [], stream: [],
      };
    },
    step() {
      frame++;
      const s = sessions[Math.floor(r() * sessions.length)];
      const changed = { id: s.id };
      const agentRows = [];
      const attRows = [];
      if (r() < 0.3) {
        // 狀態改變：舊的注意事項解除，新的（若有）進佇列
        for (const [id, a] of attention) if (a.session === s.id) { attention.delete(id); attRows.push({ id, deleted: true }); }
        s.status = STATUSES[Math.floor(r() * STATUSES.length)];
        s.statusSince = Date.now();
        Object.assign(changed, { status: s.status, statusSince: s.statusSince });
        const kind = ATTENTION_KIND[s.status];
        if (kind) {
          const a = { id: `${s.id}/${kind}/toolu_${frame}`, session: s.id, kind, ref: `toolu_${frame}`, reason: ATTENTION_REASON[kind], raisedAt: Date.now() };
          attention.set(a.id, a);
          attRows.push(a);
        }
        if (s.status === "danger") { changed.dangers = ++s.dangers; kpi.dangers++; }
        if (s.status === "fail") { changed.failures = ++s.failures; kpi.failures++; }
      }
      stepAgents(s, agentRows);
      changed.agentsLive = s.agentsLive;
      kpi.commands += 1 + Math.floor(r() * 3);
      kpi.edits += r() < 0.5 ? 1 : 0;
      kpi.writes += r() < 0.1 ? 1 : 0;
      if (r() < 0.08) { kpi.commits++; changed.commits = ++s.commits; }
      const think = Math.floor(r() * 900);
      const cost = r() * 0.02;
      s.thinkingTokens += think;
      s.costUsd = +(s.costUsd + cost).toFixed(4);
      Object.assign(changed, { thinkingTokens: s.thinkingTokens, costUsd: s.costUsd });
      kpi.thinkingTokens += think;
      kpi.costUsd = +(kpi.costUsd + cost).toFixed(4);
      recount();
      return { t: "frame", frame, ch: "*", events: [], rows: { sessions: [changed], kpi: { ...kpi }, agents: agentRows, attention: attRows } };
    },
  };
}

export function installMock(n) {
  const world = createWorld(n);
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
        this.timer = setInterval(() => this.#emit(world.step()), 400);
      }
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
