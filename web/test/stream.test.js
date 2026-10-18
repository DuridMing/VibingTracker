import { test } from "node:test";
import assert from "node:assert/strict";
import { createStream, kindOf } from "../js/stream.js";

// Event 照 contracts.md §4；這裡捏的是我們自己的 Event，不是 transcript。
const A = "aaaaaaaa-0000-0000-0000-000000000001";
const B = "bbbbbbbb-0000-0000-0000-000000000002";
let seq = 0;
let clock = 1_000;
function ev(session, type, payload, { pos = null, agent = "main" } = {}) {
  return { v: 1, session, seq: ++seq, type, agent, line: null, pos, ts_model: null, ts_exec: null, ts_seen: ++clock, host: "local", payload };
}
const called = (s, id, tool, input, extra = {}) => ev(s, "tool.called", { toolUseId: id, tool, input, status: "running", placement: "staged", danger: null, ...extra });
const placed = (s, id, pos, placement = "stream") => ev(s, "tool.placed", { toolUseId: id, placement }, { pos });
const updated = (s, id, status) => ev(s, "tool.updated", { toolUseId: id, status, durationMs: 120, error: null, interrupted: false });
const result = (s, id, ok, output = "") => ev(s, "tool.result", { toolUseId: id, ok, exitCode: ok ? 0 : 1, output, truncated: false });
const think = (s, pos, text, extra = {}) => ev(s, "assistant.thinking", { text, summarized: false, messageId: "m", model: "x", ...extra }, { pos });
const keys = (st) => st.rows.map((r) => r.key);
const row = (st, i) => { assert.ok(st.rows[i], `沒有第 ${i} 行（共 ${st.rows.length} 行）`); return st.rows[i]; };
const stagedCard = (st, id) => { assert.ok(st.staged.get(id), `暫存區沒有 ${id}`); return st.staged.get(id); };

test("hook 先到的卡片進暫存區，transcript 的 tool_use 行到了才依 pos 進串流", () => {
  const st = createStream();
  let ops = st.apply([called(A, "t1", "Bash", { command: "cargo test" })]);
  assert.equal(st.rows.length, 0);
  assert.equal(st.staged.size, 1);
  assert.equal(ops.stagedChanged, true);
  ops = st.apply([placed(A, "t1", 3)]);
  assert.equal(st.staged.size, 0);
  assert.equal(ops.appended, 1);
  assert.equal(row(st, 0).card.tool, "Bash");
  assert.equal(row(st, 0).card.input.command, "cargo test");
  assert.equal(row(st, 0).pos, 3);
});

test("卡片狀態更新就地改內容：不產生新行、不搬位置", () => {
  const st = createStream();
  st.apply([called(A, "t1", "Bash", { command: "ls" }), placed(A, "t1", 1), think(B, 1, "想一下")]);
  const before = keys(st);
  const ops = st.apply([updated(A, "t1", "done"), result(A, "t1", true, "a\nb")]);
  assert.deepEqual(keys(st), before);
  assert.equal(ops.appended, 0);
  assert.deepEqual([...ops.updated], [row(st, 0).key]);
  assert.equal(row(st, 0).card.status, "done");
  assert.equal(row(st, 0).card.result.output, "a\nb");
});

test("暫存區的卡片也會吃到更新，進串流時直接是完成狀態", () => {
  const st = createStream();
  st.apply([called(A, "t1", "Read", { file_path: "a.ts" }), updated(A, "t1", "done")]);
  assert.equal(stagedCard(st, "t1").status, "done");
  st.apply([placed(A, "t1", 2)]);
  assert.equal(row(st, 0).card.status, "done");
});

test("暫存提示列計數隨 staged→stream 正確增減", () => {
  const st = createStream();
  st.apply([called(A, "t1", "Bash", { command: "a" }), called(A, "t2", "Bash", { command: "b" }), called(B, "t3", "Read", { file_path: "x" })]);
  assert.equal(st.staged.size, 3);
  st.apply([placed(A, "t1", 1)]);
  assert.equal(st.staged.size, 2);
  st.apply([placed(B, "t3", 1, "fallback"), placed(A, "t2", 2)]);
  assert.equal(st.staged.size, 0);
});

test("跨 session 依到達順序 append；重複的 (session, pos) 不會出現兩次", () => {
  const st = createStream();
  st.apply([think(A, 1, "a1"), think(B, 1, "b1"), think(A, 2, "a2")]);
  st.apply([think(A, 2, "a2 重送"), think(B, 2, "b2")]);
  assert.deepEqual(st.rows.map((r) => r.text), ["a1", "b1", "a2", "b2"]);
});

test("超過上限從最舊的開始丟，回報丟掉的行數", () => {
  const st = createStream({ cap: 5 });
  st.apply([1, 2, 3, 4].map((p) => think(A, p, `t${p}`)));
  const ops = st.apply([5, 6, 7].map((p) => think(A, p, `t${p}`)));
  assert.equal(ops.dropped, 2);
  assert.deepEqual(st.rows.map((r) => r.text), ["t3", "t4", "t5", "t6", "t7"]);
});

test("篩選只影響可見性，不改順序；取消篩選後完全恢復", () => {
  const st = createStream();
  st.apply([think(A, 1, "a1"), called(B, "t1", "Bash", { command: "x" }), placed(B, "t1", 1), think(A, 2, "a2"), think(B, 2, "b2")]);
  const order = keys(st);
  st.setSessionHidden(B, true);
  assert.deepEqual(st.rows.filter((r) => st.isVisible(r)).map((r) => r.key), [order[0], order[2]]);
  st.setKindHidden("th", true);
  assert.equal(st.rows.filter((r) => st.isVisible(r)).length, 0);
  st.setSessionHidden(B, false);
  st.setKindHidden("th", false);
  assert.deepEqual(keys(st), order);
  assert.ok(st.rows.every((r) => st.isVisible(r)));
});

test("降級卡片標 fallback，之後補上的思考帶 delayed", () => {
  const st = createStream();
  st.apply([called(A, "t1", "Bash", { command: "x" }), placed(A, "t1", 1, "fallback"), think(A, 2, "晚到的思考", { delayed: true })]);
  assert.equal(row(st, 0).card.fallback, true);
  assert.equal(row(st, 1).delayed, true);
  st.apply([think(A, 3, "正常")]);
  assert.equal(row(st, 2).delayed, false);
});

test("transcript 先到（tool.called 帶 pos）直接進串流、狀態 pending", () => {
  const st = createStream();
  st.apply([ev(A, "tool.called", { toolUseId: "t1", tool: "Bash", input: { command: "x" }, status: "pending", placement: "stream", danger: null }, { pos: 4 })]);
  assert.equal(st.staged.size, 0);
  assert.equal(row(st, 0).card.status, "pending");
  st.apply([updated(A, "t1", "running")]);
  assert.equal(st.rows.length, 1);
  assert.equal(row(st, 0).card.status, "running");
});

test("危險卡片帶 danger；prompt 與等你／完成狀態出現在串流，run 不出現", () => {
  const st = createStream();
  st.apply([
    called(A, "t1", "Bash", { command: "git push --force" }, { danger: { rule: "force-push", level: "danger", why: "強制推送" } }),
    placed(A, "t1", 1),
    ev(A, "prompt.submitted", { text: "修 bug", promptId: "p1" }),
    ev(A, "session.status", { status: "run", since: 1, score: 0 }),
    ev(A, "session.status", { status: "wait", since: 2, score: 0 }),
    ev(A, "session.status", { status: "done", since: 3, score: 0 }),
  ]);
  assert.equal(row(st, 0).card.danger.why, "強制推送");
  assert.deepEqual(st.rows.slice(1).map((r) => r.status), ["prompt", "wait", "done"]);
  assert.ok(st.rows.slice(1).every((r) => kindOf(r) === "st"));
});

test("衍生事件補進卡片：Edit 的增減行數、git commit、派出的 subagent", () => {
  const st = createStream();
  st.apply([
    called(A, "e1", "Edit", { file_path: "calc.py" }), placed(A, "e1", 1),
    ev(A, "file.edit", { toolUseId: "e1", path: "calc.py", hunk: { start: 5, oldLines: 3, newLines: 3, lines: [" def sub(a, b):", "-    return a + b", "+    return a - b"] }, hunkSource: "pre" }),
    called(A, "g1", "Bash", { command: "git commit -m x" }), placed(A, "g1", 2),
    ev(A, "git.op", { toolUseId: "g1", op: "commit", branch: "main", hash: "4cf93bc", message: "修正 sub", ok: true }),
    called(A, "k1", "Task", { description: "找 .md" }), placed(A, "k1", 3),
    ev(A, "agent.spawned", { agentId: "a1", agentType: "Explore", name: "找 .md 檔", task: "列出 .md", parentToolUseId: "k1" }),
  ]);
  assert.deepEqual(row(st, 0).card.edit, { path: "calc.py", added: 1, removed: 1, countsKnown: true, countLabel: null });
  assert.equal(row(st, 1).card.git.hash, "4cf93bc");
  assert.equal(row(st, 2).card.agentName, "找 .md 檔");
});

test("類型分類", () => {
  const st = createStream();
  const tools = ["Read", "Grep", "Edit", "Write", "Task", "Bash", "WebFetch"];
  st.apply(tools.flatMap((t, i) => [called(A, `x${i}`, t, {}), placed(A, `x${i}`, i + 1)]));
  st.apply([think(A, 20, "t"), ev(A, "assistant.text", { text: "hi", messageId: "m", model: "x" }, { pos: 21 })]);
  assert.deepEqual(st.rows.map(kindOf), ["rd", "rd", "ed", "wr", "wr", "cmd", "rd", "th", "tx"]);
});

test("snapshot 重建：staged 進暫存區，stream 照陣列順序（含補充卡片內容的 Event）", () => {
  const st = createStream();
  st.apply([think(A, 99, "舊的")]);
  st.load({
    staged: [called(B, "s1", "Bash", { command: "sleep 1" })],
    stream: [think(A, 1, "a1"), called(B, "t1", "Read", { file_path: "x" }), placed(B, "t1", 1), updated(B, "t1", "done"), think(A, 2, "a2")],
  });
  assert.deepEqual(st.rows.map((r) => r.text ?? r.card.tool), ["a1", "Read", "a2"]);
  assert.equal(row(st, 1).card.status, "done");
  assert.equal(st.staged.size, 1);
});

test("載入更多：各 session 的結果依 (ts_seen, session, pos) 合併後放在最上面", () => {
  const st = createStream({ cap: 4 });
  const old = { a1: think(A, 1, "a1"), b1: think(B, 1, "b1"), a2: think(A, 2, "a2") }; // 依 ts_seen：a1 < b1 < a2
  st.apply([think(A, 3, "a3"), think(B, 2, "b2")]);
  assert.deepEqual(st.minPos(), new Map([[A, 3], [B, 2]]));
  const res = st.prepend([[old.a1, old.a2], [old.b1, think(B, 2, "b2 重複")]]);
  assert.equal(res.prepended, 3);
  assert.deepEqual(st.rows.map((r) => r.text), ["a1", "b1", "a2", "a3", "b2"]);
  assert.equal(st.holding, true);
  // 往上看歷史時暫時超過上限；回到底部後修剪回上限
  st.apply([think(A, 4, "a4")]);
  assert.equal(st.rows.length, 6);
  const t = st.trim();
  assert.equal(t.dropped, 2);
  assert.equal(st.holding, false);
  assert.deepEqual(st.rows.map((r) => r.text), ["a2", "a3", "b2", "a4"]);
});

test("被丟掉的卡片之後的更新不會讓它復活", () => {
  const st = createStream({ cap: 1 });
  st.apply([called(A, "t1", "Bash", { command: "x" }), placed(A, "t1", 1), think(A, 2, "新")]);
  const ops = st.apply([updated(A, "t1", "done")]);
  assert.deepEqual(st.rows.map((r) => r.text), ["新"]);
  assert.equal(ops.updated.size, 0);
  assert.equal(st.staged.size, 0);
});

test("session 結束時清掉它還在暫存區的卡片，其他 session 的不動", () => {
  const st = createStream();
  st.apply([called(A, "t1", "Bash", { command: "sleep 100" }), called(B, "t2", "Read", { file_path: "x" })]);
  const ops = st.apply([ev(A, "session.ended", { reason: "other" })]);
  assert.equal(ops.stagedChanged, true);
  assert.deepEqual([...st.staged.keys()], ["t2"]);
  // 已經清掉的卡片之後就算 tool.placed 晚到，也照常進串流（transcript 補上的就是事實）
  st.apply([placed(A, "t1", 5)]);
  assert.equal(row(st, st.rows.length - 1).card.toolUseId, "t1");
});

test("按 Esc 中斷（session.status idle）在串流出現狀態行；背景工作完成的 prompt 帶 source", () => {
  const st = createStream();
  st.apply([
    ev(A, "session.status", { status: "idle", since: 1, score: 0 }),
    ev(A, "prompt.submitted", { text: "<task-notification>build done</task-notification>", promptId: "p2", source: "task" }),
    ev(A, "prompt.submitted", { text: "繼續", promptId: "p3" }),
  ]);
  assert.deepEqual(st.rows.map((r) => r.status), ["idle", "prompt", "prompt"]);
  assert.equal(row(st, 1).source, "task");
  assert.equal(row(st, 2).source, "user");
});

test("subagent 結束（agent.finished）在串流出現一行狀態：完成、失敗、被停掉分開；同一筆不重複", () => {
  const st = createStream();
  const fin = (agentId, status, last = "結果") => ev(A, "agent.finished", { agentId, lastMessage: last, transcriptPath: null, ...(status ? { status } : {}) }, { agent: agentId });
  const f1 = fin("a1", undefined, "找到 3 個呼叫點");
  st.apply([f1, fin("a2", "completed"), fin("a3", "killed"), fin("a4", "failed")]);
  st.apply([f1]);
  const rows = st.rows.filter((r) => r.type === "status");
  assert.deepEqual(rows.map((r) => [r.status, r.agentId, r.result]), [["agent", "a1", "done"], ["agent", "a2", "done"], ["agent", "a3", "killed"], ["agent", "a4", "fail"]]);
  assert.equal(rows[0].text, "找到 3 個呼叫點");
  assert.equal(kindOf(rows[0]), "st");
});

test("resume／compact 在串流出現一行狀態（#651）；startup 與 clear（新 session 的第一行，#688）不出現；同一筆不重複", () => {
  const st = createStream();
  const start = (source) => ev(A, "session.started", { cwd: "/p", source, model: null, transcriptPath: "/t", branch: "main" });
  const r1 = start("resume");
  st.apply([start("startup"), r1, start("compact"), start("clear")]);
  st.apply([r1]);
  const rows = st.rows.filter((r) => r.type === "status");
  assert.deepEqual(rows.map((r) => [r.status, r.source]), [["session", "resume"], ["session", "compact"]]);
  assert.equal(kindOf(rows[0]), "st");
});

test("transcript 先到時危險標示走 tool.updated：卡片要帶上 danger，之後的更新不清掉（#681）", () => {
  const st = createStream();
  const danger = { rule: "rm-rf", level: "danger", why: "刪除" };
  st.apply([ev(A, "tool.called", { toolUseId: "t1", tool: "Bash", input: { command: "rm -rf x" }, status: "pending", placement: "stream", danger: null }, { pos: 1 })]);
  st.apply([ev(A, "tool.updated", { toolUseId: "t1", status: "running", durationMs: null, error: null, interrupted: null, danger })]);
  assert.deepEqual(row(st, 0).card.danger, danger);
  st.apply([updated(A, "t1", "done")]);
  assert.deepEqual(row(st, 0).card.danger, danger);
  assert.equal(row(st, 0).card.status, "done");
  // snapshot 的伴隨 Event 順序不保證：tool.updated 先於 tool.called 也不能被蓋回 null
  const st2 = createStream();
  st2.load({ stream: [
    ev(A, "tool.updated", { toolUseId: "t2", status: "done", durationMs: 1, error: null, interrupted: null, danger }),
    ev(A, "tool.called", { toolUseId: "t2", tool: "Bash", input: { command: "rm -rf x" }, status: "pending", placement: "stream", danger: null }, { pos: 1 }),
  ] });
  assert.deepEqual(row(st2, 0).card.danger, danger);
});

test("載入更多：同 session 內照 server 給的順序（依 pos），不因 ts_seen 倒退而重排（#681）", () => {
  const st = createStream();
  st.apply([think(A, 10, "a10")]);
  const a1 = think(A, 1, "a1");
  const a2 = think(A, 2, "a2");
  a2.ts_seen = a1.ts_seen - 5; // 同 session 內 ts_seen 倒退
  const b1 = think(B, 1, "b1");
  b1.ts_seen = a1.ts_seen - 1;
  const p = ev(A, "prompt.submitted", { text: "下一個", promptId: "p" });
  p.ts_seen = a1.ts_seen + 1;
  st.prepend([[a1, a2, p], [b1]]);
  assert.deepEqual(st.rows.slice(0, 4).map((r) => r.text), ["b1", "a1", "a2", "下一個"]);
});

test("Write 卡片的行數用 file.write 的 lines（全文），沒有才數預覽（#682）", () => {
  const st = createStream();
  const w = (id, extra) => ({ v: 1, session: "s", seq: id.length, type: "file.write", agent: "main", line: null, pos: null, ts_model: null, ts_exec: null, ts_seen: 1, host: "local",
    payload: { toolUseId: id, path: "big.txt", isNew: true, content: "a\nb\n", truncated: true, ...extra } });
  st.apply([w("w1", { lines: 5000 })]);
  const card = (id) => [...st.staged.values(), ...st.rows.map((r) => r.card)].find((c) => c?.toolUseId === id);
  // file.write 先到時卡片還沒進暫存區，從內部的卡片表查：用 tool.called 建出來
  st.apply([{ ...w("w1", {}), type: "tool.called", seq: 99, payload: { toolUseId: "w1", tool: "Write", input: {}, status: "running", placement: "staged", danger: null } }]);
  assert.equal(card("w1").write.lines, 5000);
});

test("專案篩選（#669）：setSessionFilter 只影響可見性，不動 rows；清掉後全部可見", () => {
  const st = createStream();
  st.apply([called(A, "p1", "Bash", { command: "a" }), placed(A, "p1", 1), called(B, "p2", "Bash", { command: "b" }), placed(B, "p2", 1)]);
  st.setSessionFilter((id) => id === A);
  assert.equal(st.rows.length, 2, "行還在");
  assert.deepEqual(st.rows.filter((r) => st.isVisible(r)).map((r) => r.session), [A]);
  st.setSessionHidden(A, true);
  assert.equal(st.rows.filter((r) => st.isVisible(r)).length, 0, "和 session 篩選疊加");
  st.setSessionHidden(A, false);
  st.setSessionFilter(null);
  assert.ok(st.rows.every((r) => st.isVisible(r)));
});

test("展開中的卡片收到結果：高度清掉讓畫面重量，下面的行不會疊上來（#747 F-1）", () => {
  const st = createStream();
  st.apply([called(A, "t1", "Bash", { command: "make" }), placed(A, "t1", 1)]);
  const r = row(st, 0);
  r.expanded = true;
  r.h = 40;
  st.apply([result(A, "t1", true, "a\nb\nc")]);
  assert.equal(r.h, 0, "展開中的行內容變了，要重新量高度");
  const other = row(st, 0);
  other.expanded = false;
  other.h = 22;
  st.apply([updated(A, "t1", "done")]);
  assert.equal(other.h, 22, "沒展開的行高度不變，不必重量");
});

test("載入更多後停在上方（holding）時串流仍有硬上限：cap 的 3 倍（#766 F-4）", () => {
  const st = createStream({ cap: 100 });
  const batch = (n, start) => Array.from({ length: n }, (_, i) => think(A, start + i, "x"));
  st.apply(batch(100, 1000));
  st.prepend([batch(10, 900)]);
  assert.equal(st.holding, true);
  for (let k = 0; k < 50; k++) st.apply(batch(100, 2000 + k * 100));
  assert.ok(st.rows.length <= 300, `holding 時留了 ${st.rows.length} 行`);
  assert.equal(st.rows.at(-1).pos, 2000 + 49 * 100 + 99, "最新的行一定留著");
});
