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

// collector 端的狀態在所有連線之間共用，斷線重連後還是同一批 session。
function createWorld(n) {
  const r = rng(20261016);
  const sessions = Array.from({ length: n }, (_, i) => ({
    id: uuid(r),
    status: "run",
    prompt: PROMPTS[i % PROMPTS.length],
    branch: i % 3 === 0 ? "develop" : `feature/t${i}`,
    model: i % 4 === 0 ? "opus" : "sonnet",
    commits: 0,
    dangers: 0,
    agents: 1 + (i % 3),
  }));
  const kpi = { live: n, attention: 0, sessions: n, agents: 0, edits: 0, writes: 0, commands: 0, failures: 0, dangers: 0, commits: 0, thinkingTokens: 0, costUsd: 0 };
  let frame = 0;

  function recount() {
    kpi.live = sessions.filter((s) => s.status === "run").length;
    kpi.attention = sessions.filter((s) => s.status !== "run").length;
    kpi.agents = sessions.reduce((a, s) => a + s.agents - 1, 0);
  }
  recount();

  return {
    snapshot() {
      return { t: "snapshot", ch: "*", frame, sessions: sessions.map((s) => ({ ...s })), kpi: { ...kpi } };
    },
    step() {
      frame++;
      const s = sessions[Math.floor(r() * sessions.length)];
      const changed = { id: s.id };
      if (r() < 0.35) {
        s.status = STATUSES[Math.floor(r() * STATUSES.length)];
        changed.status = s.status;
        if (s.status === "danger") changed.dangers = ++s.dangers;
      }
      kpi.commands += 1 + Math.floor(r() * 3);
      kpi.edits += r() < 0.5 ? 1 : 0;
      kpi.writes += r() < 0.1 ? 1 : 0;
      kpi.failures += s.status === "fail" ? 1 : 0;
      kpi.dangers += s.status === "danger" ? 1 : 0;
      if (r() < 0.08) { kpi.commits++; changed.commits = ++s.commits; }
      kpi.thinkingTokens += Math.floor(r() * 900);
      kpi.costUsd = +(kpi.costUsd + r() * 0.02).toFixed(4);
      recount();
      return { t: "frame", frame, ch: "*", events: [], rows: { sessions: [changed], kpi: { ...kpi } } };
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
