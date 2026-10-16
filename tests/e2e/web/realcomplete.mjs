// 唯讀：用前端的 client／stream 模型連真 collector，對事件最多的幾個 session 收 snapshot、一路「載入更多」到 pos 1，
// 再收 3 秒即時 frame，然後和 collector 的 JSONL（<CODING_LAB_HOME>/events/<id>.jsonl）裡每個帶 pos 的 Event 逐一比對：
// 缺漏、多出、重複都算失敗（#639 驗收「三個 session 同時跑不漏任何一行」）。
// JSONL 裡同一個 pos 出現多次（#670 修好之前 collector 重啟會從 1 重編）的舊資料比不準，用「起始時間」只比那之後的 Event。
// 跑法：tests/scripts/web-real.sh complete [session 數，預設 3] [起始時間 epoch ms，預設 0]
import { readFileSync, readdirSync } from "node:fs";
import { createClient } from "/web/js/client.js";
import { createStream } from "/web/js/stream.js";

const EVD = process.argv[2];
const N = Number(process.argv[3] ?? 3);
const SINCE = Number(process.argv[4] ?? 0);
const st = createStream({ cap: 1e9 }); // 不丟舊行，才能比完整
const files = new Set(readdirSync(EVD));

function jsonlPos(id) {
  const pos = [];
  for (const line of readFileSync(`${EVD}/${id}.jsonl`, "utf8").split("\n")) {
    if (!line) continue;
    try {
      const e = JSON.parse(line);
      if (e.pos != null && e.ts_seen >= SINCE) pos.push(e.pos);
    } catch {
      // 寫到一半的最後一行：下次再讀
    }
  }
  return pos;
}
const streamPos = (id) => st.rows.filter((r) => r.session === id && r.pos != null).map((r) => r.pos);
const minPos = (id) => Math.min(...streamPos(id));

let targets = [];
const reached = new Set();
let snapshotSeen = false;
let finishing = false;

function nextMore(c) {
  for (const id of targets) {
    const m = minPos(id);
    if (Number.isFinite(m) && m > 1 && !reached.has(id)) {
      c.more({ session: id, beforePos: m, limit: 500 });
      return true;
    }
  }
  return false;
}

function finish() {
  if (finishing) return;
  finishing = true;
  setTimeout(() => {
    let bad = 0;
    for (const id of targets) {
      const jp = jsonlPos(id);
      if (!jp.length) continue;
      const from = Math.min(...jp);
      const sp = streamPos(id).filter((p) => p >= from);
      const maxSeen = Math.max(...sp);
      const want = jp.filter((p) => p <= maxSeen); // 讀檔時才寫進來、還沒推到前端的不算
      const js = new Set(want);
      const ss = new Set(sp);
      const missing = [...js].filter((p) => !ss.has(p));
      const extra = [...ss].filter((p) => !js.has(p));
      const jdup = want.length - js.size;
      const sdup = sp.length - ss.size;
      const ok = missing.length === 0 && extra.length === 0 && sdup === 0;
      if (!ok) bad++;
      console.log(`${ok ? "OK  " : "FAIL"} ${id.slice(0, 8)} jsonl=${want.length}${jdup ? `（重號 ${jdup}，舊資料比不準，加起始時間）` : ""} 串流=${sp.length}（重複 ${sdup}） 缺 ${missing.length}${missing.length ? ` ${missing.slice(0, 10).join(",")}` : ""} 多 ${extra.length} pos ${from}–${maxSeen}`);
    }
    console.log(bad ? `FAIL：${bad} 個 session 不一致` : `全部一致（${targets.length} 個 session）`);
    process.exit(bad ? 1 : 0);
  }, 3000);
}

const c = createClient({
  url: "ws://127.0.0.1:4801/ws",
  WebSocket,
  setTimeout,
  clearTimeout,
  utcOffsetMin: -new Date().getTimezoneOffset(),
  handlers: {
    snapshot: (m) => {
      if (m.ch !== "*" || snapshotSeen) return;
      snapshotSeen = true;
      st.load({ staged: m.staged, stream: m.stream });
      targets = (m.sessions ?? [])
        .map((s) => s.id)
        .filter((id) => files.has(`${id}.jsonl`))
        .map((id) => [id, jsonlPos(id).length])
        .filter(([, n]) => n > 0)
        .sort((a, b) => b[1] - a[1])
        .slice(0, N)
        .map(([id]) => id);
      if (!targets.length) {
        console.log("FAIL：沒有可比對的 session（起始時間太晚？）");
        process.exit(1);
      }
      if (!nextMore(c)) finish();
    },
    frame: (m) => { if (m.ch === "*") st.apply(m.events ?? []); },
    more: (m) => {
      if (!(m.events ?? []).some((e) => e.pos != null)) reached.add(m.session);
      st.prepend([m.events ?? []]);
      if (minPos(m.session) <= 1) reached.add(m.session);
      if (!nextMore(c)) finish();
    },
    welcome() {},
    onStatus() {},
    onIncompatible() { console.log("FAIL：協定版本不符"); process.exit(1); },
    onError() {},
  },
});
c.subscribe("*");
c.connect();
setTimeout(() => { console.log("FAIL：逾時（collector 沒回應？）"); process.exit(2); }, 90_000);
