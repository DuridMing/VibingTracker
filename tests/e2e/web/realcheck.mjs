// 唯讀：用前端實際的模組（client／store／stream／format）連真的 collector（ws://127.0.0.1:4801），
// 核對 snapshot 與 frame 是否符合契約 §6 與前端的預期：欄位齊全、每個 session 的 pos 只增不減、
// 沒有重複行、每一行都能格式化、卡片都有伴隨的 tool.called。有問題 exit 1。
// 跑法：tests/scripts/web-real.sh check [秒數]（預設 20 秒）。
import { createClient } from "/web/js/client.js";
import { createStore, applySnapshot, applyFrame } from "/web/js/store.js";
import { createStream, kindOf } from "/web/js/stream.js";
import { formatRow } from "/web/js/format.js";

const SECS = Number(process.argv[2] ?? 20);
const store = createStore();
const st = createStream();
const notes = [];
const types = new Map();
let welcome = null;
let snapKeys = null;
let frames = 0;
let events = 0;
let moreRes = null;

const count = (evs) => { for (const e of evs ?? []) types.set(e.type, (types.get(e.type) ?? 0) + 1); };
function checkOrder(label) {
  const last = new Map();
  const seen = new Set();
  for (const r of st.rows) {
    if (seen.has(r.key)) notes.push(`${label}: 重複行 ${r.key}`);
    seen.add(r.key);
    if (r.pos == null) continue;
    const p = last.get(r.session) ?? 0;
    if (r.pos <= p) notes.push(`${label}: ${r.session.slice(0, 8)} pos ${r.pos} 出現在 ${p} 之後`);
    last.set(r.session, r.pos);
  }
  for (const r of st.rows) { try { formatRow(r); } catch (e) { notes.push(`formatRow 例外 ${r.key}: ${e.message}`); } }
  const empty = st.rows.filter((r) => r.type === "tool" && r.card.tool == null).length;
  if (empty) notes.push(`${label}: ${empty} 張卡片缺 tool.called（顯示「內容不可用」）`);
}

const c = createClient({
  url: "ws://127.0.0.1:4801/ws", WebSocket: globalThis.WebSocket, setTimeout, clearTimeout,
  utcOffsetMin: 480,
  handlers: {
    welcome: (m) => { welcome = m; },
    snapshot: (m) => {
      if (m.ch !== "*") return;
      snapKeys = Object.keys(m).sort();
      applySnapshot(store, m);
      st.load(m);
      count(m.stream); count(m.staged);
      checkOrder("snapshot");
      // 請一次「載入更多」，確認回應格式
      const want = [...st.minPos()].filter(([, p]) => p > 1);
      if (want.length) c.more({ session: want[0][0], beforePos: want[0][1], limit: 20 });
    },
    frame: (m) => {
      if (m.ch !== "*") return;
      frames++;
      events += m.events?.length ?? 0;
      count(m.events);
      applyFrame(store, m);
      st.apply(m.events);
    },
    more: (m) => { moreRes = { keys: Object.keys(m).sort(), n: m.events?.length ?? 0, session: m.session?.slice(0, 8), beforePos: m.beforePos }; },
    onError: (e) => notes.push(`訊息解析錯誤 ${e.message}`),
    onIncompatible: (m) => notes.push(`協定不相容 ${JSON.stringify(m)}`),
    onStatus: (s) => { if (s === "closed") notes.push("連線關閉"); },
  },
});
c.subscribe("*");
c.connect();

setTimeout(() => {
  checkOrder("after frames");
  const sess = [...store.sessions.values()];
  const sample = sess[0] ?? {};
  const want = ["id","host","cwd","branch","model","firstPrompt","lastPrompt","startedAt","endedAt","status","statusSince","doing","commits","dangers","failures","agentsLive","costUsd","thinkingTokens","lastPos"];
  const missing = want.filter((k) => !(k in sample));
  const kinds = {};
  for (const r of st.rows) kinds[kindOf(r)] = (kinds[kindOf(r)] ?? 0) + 1;
  console.log("welcome:", JSON.stringify(welcome));
  console.log("snapshot keys:", snapKeys?.join(","));
  console.log(`sessions=${sess.length} statuses=${sess.map((s) => s.status).join(",")} agents=${store.agents.size} attention=${store.attention.size}`);
  console.log("session row missing fields:", missing.length ? missing.join(",") : "(none)");
  console.log("kpi:", JSON.stringify(store.kpi));
  console.log(`stream rows=${st.rows.length} staged=${st.staged.size} kinds=${JSON.stringify(kinds)}`);
  console.log(`frames=${frames} events=${events} in ${SECS}s`);
  console.log("event types:", [...types].map(([t, n]) => `${t}:${n}`).join(" "));
  console.log("more:", JSON.stringify(moreRes));
  console.log("status values seen:", [...new Set(st.rows.filter((r) => r.type === "status").map((r) => r.status))].join(","));
  console.log("prompt sources:", JSON.stringify([...new Set(st.rows.filter((r) => r.status === "prompt").map((r) => r.source ?? "user"))]));
  console.log(notes.length ? "NOTES:\n" + [...new Set(notes)].slice(0, 30).join("\n") : "NOTES: none");
  c.close();
  process.exit(notes.length ? 1 : 0);
}, SECS * 1000);
