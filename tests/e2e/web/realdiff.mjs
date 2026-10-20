// 唯讀：取真 collector 改動紀錄裡的 Edit，用前端的 diffRows 算出新檔那一側的行號與內容，
// 和目前的檔案比對（#640 驗收「與 git diff 抽查一致」）。檔案之後又被改過時會標出來，不算失敗。
// 沒有任何 Edit 可比、或連不上時 exit 1。
// 跑法：tests/scripts/web-real.sh diff [筆數]（預設 20 筆；需要把專案目錄唯讀掛進容器，腳本會處理）。
import { readFileSync } from "node:fs";
import { createClient } from "/web/js/client.js";
import { createChanges, diffRows } from "/web/js/changes.js";

const ch = createChanges();
const cwdOf = new Map();
const c = createClient({
  url: "ws://127.0.0.1:4801/ws", WebSocket: globalThis.WebSocket, setTimeout, clearTimeout,
  handlers: {
    snapshot: (m) => {
      if (m.ch !== "*") return;
      for (const s of m.sessions ?? []) cwdOf.set(s.id, s.cwd);
      ch.load(m.changes);
      report();
    },
  },
});
c.subscribe("*");
c.connect();

function report() {
  const edits = ch.list.filter((e) => e.kind === "edit");
  console.log(`changes=${ch.list.length} edits=${edits.length} writes=${ch.list.length - edits.length}`);
  let ok = 0, moved = 0, nohunk = 0, missing = 0;
  for (const e of edits.slice(0, Number(process.argv[2] ?? 20))) {
    const file = e.path.startsWith("/") ? e.path : `${cwdOf.get(e.session) ?? ""}/${e.path}`;
    if (!e.hunk) { nohunk++; console.log(`  NOHUNK ${e.path} (${e.hunkSource})`); continue; }
    let lines;
    try { lines = readFileSync(file, "utf8").split("\n"); } catch { missing++; console.log(`  MISSING ${file}`); continue; }
    const rows = diffRows(e.hunk, e.hunkSource).filter((r) => r.new != null);
    const bad = rows.filter((r) => lines[r.new - 1] !== r.text);
    const hdr = diffRows(e.hunk)[0].text;
    if (!bad.length) { ok++; console.log(`  OK     ${e.path} ${hdr} (${rows.length} 行新檔內容一致)`); }
    else {
      // 之後又被改過：看新內容是否整段出現在別的位置
      const want = rows.map((r) => r.text).join("\n");
      const at = lines.join("\n").indexOf(want);
      moved++;
      console.log(`  DIFF   ${e.path} ${hdr} 不一致 ${bad.length}/${rows.length} 行${at >= 0 ? `（整段出現在第 ${lines.join("\n").slice(0, at).split("\n").length} 行，檔案之後被改過、行號位移）` : "（檔案之後被改過）"}`);
    }
  }
  console.log(`summary: ok=${ok} changed-later=${moved} nohunk=${nohunk} missing=${missing}`);
  c.close();
  process.exit(ok + moved + nohunk > 0 ? 0 : 1);
}
setTimeout(() => { console.log("timeout"); process.exit(1); }, 10000);
