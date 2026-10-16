// 冒煙測試：mock collector（一般模式與 bench 模式）→ client → stream 模型，不需要 DOM。
// 檢查每個 session 的 pos 只增不減、沒有重複行、每一行都能格式化，並在 bench 模式試一次「載入更多」。
// 有問題 exit 1。跑法：tests/scripts/web-mocksmoke.sh。
import { createClient } from "/web/js/client.js";
import { installMock } from "/web/js/mock-feed.js";
import { createStream } from "/web/js/stream.js";
import { formatRow } from "/web/js/format.js";

const problems = [];
function checkOrder(st, label) {
  const last = new Map();
  const seen = new Set();
  for (const r of st.rows) {
    if (seen.has(r.key)) problems.push(`${label}: 重複 ${r.key}`);
    seen.add(r.key);
    if (r.pos == null) continue;
    const p = last.get(r.session) ?? 0;
    if (r.pos <= p) problems.push(`${label}: ${r.session.slice(0, 8)} pos ${r.pos} 在 ${p} 之後`);
    last.set(r.session, r.pos);
  }
  for (const r of st.rows) formatRow(r); // 每種行都能格式化、不丟例外
}

for (const bench of [false, true]) {
  const mock = installMock(16, { bench });
  const st = createStream();
  let frames = 0;
  let maxStaged = 0;
  let pendingMore = 0;
  const got = [];
  const stats = { fallback: 0, delayed: 0, danger: 0, status: 0, kinds: new Set() };
  await new Promise((done) => {
    const c = createClient({
      url: "ws://x/ws", WebSocket: mock.WebSocket, setTimeout, clearTimeout,
      handlers: {
        snapshot: (m) => { st.load(m); checkOrder(st, `${bench ? "bench" : "normal"} snapshot`); },
        frame: (m) => {
          frames++;
          st.apply(m.events);
          maxStaged = Math.max(maxStaged, st.staged.size);
          if (frames === 6) {
            const want = [...st.minPos()].filter(([, p]) => p > 1);
            pendingMore = want.length;
            for (const [session, beforePos] of want) c.more({ session, beforePos, limit: 50 });
          }
        },
        more: (m) => {
          got.push(m.events);
          if (--pendingMore === 0) {
            const before = st.rows.length;
            const res = st.prepend(got); console.log(`more: sessions=${got.length} prepended=${res.prepended} rows=${st.rows.length}`);
            if (st.rows.length !== before + res.prepended) problems.push("prepend 行數不符");
            checkOrder(st, "after more");
          }
        },
      },
    });
    c.subscribe("*");
    c.connect();
    setTimeout(() => {
      checkOrder(st, bench ? "bench" : "normal");
      for (const r of st.rows) {
        if (r.card?.fallback) stats.fallback++;
        if (r.delayed) stats.delayed++;
        if (r.card?.danger) stats.danger++;
        if (r.type === "status") stats.status++;
        stats.kinds.add(r.card?.tool ?? r.type);
      }
      console.log(`${bench ? "bench " : "normal"}: frames=${frames} rows=${st.rows.length} holding=${st.holding} maxStaged=${maxStaged} fallback=${stats.fallback} delayed=${stats.delayed} danger=${stats.danger} status=${stats.status} kinds=${[...stats.kinds].join(",")}`);
      c.close();
      done();
    }, bench ? 2500 : 5000);
  });
}
console.log(problems.length ? "PROBLEMS:\n" + [...new Set(problems)].slice(0, 20).join("\n") : "order OK");
process.exit(problems.length ? 1 : 0);
