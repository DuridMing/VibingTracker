import { test } from "node:test";
import assert from "node:assert/strict";
import { createChanges, diffRows, writeRows, countHunk } from "../js/changes.js";

// file.edit／file.write Event 照 contracts.md §4
const A = "aaaaaaaa-0000-0000-0000-000000000001";
const B = "bbbbbbbb-0000-0000-0000-000000000002";
let seq = 0;
function ev(session, type, payload, agent = "main") {
  return { v: 1, session, seq: ++seq, type, agent, line: null, pos: null, ts_model: null, ts_exec: null, ts_seen: 1000 + seq, host: "local", payload };
}
const edit = (s, path, hunk = { start: 5, oldLines: 1, newLines: 1, lines: ["-a", "+b"] }) => ev(s, "file.edit", { toolUseId: `t${seq}`, path, hunk, hunkSource: "pre" });
const write = (s, path, content = "x\n", extra = {}) => ev(s, "file.write", { toolUseId: `t${seq}`, path, isNew: true, content, truncated: false, ...extra });
const paths = (ch) => ch.list.map((c) => c.path);
const sel = (ch) => { const c = ch.selected(); assert.ok(c, "沒有選中的改動"); return c; };
const at = (ch, i) => { assert.ok(ch.list[i], `改動紀錄沒有第 ${i} 筆（共 ${ch.list.length} 筆）`); return ch.list[i]; };

test("跨 session、最新在上；其他類型的 Event 不進改動紀錄", () => {
  const ch = createChanges();
  ch.apply([edit(A, "a.ts"), ev(A, "file.read", { toolUseId: "r", path: "x" }), write(B, "b.ts"), edit(A, "c.ts")]);
  assert.deepEqual(paths(ch), ["c.ts", "b.ts", "a.ts"]);
  assert.equal(ch.list[1].kind, "write");
  assert.equal(ch.list[1].session, B);
});

test("最多 60 筆，超過丟最舊的", () => {
  const ch = createChanges();
  ch.apply(Array.from({ length: 65 }, (_, i) => edit(A, `f${i}.ts`)));
  assert.equal(ch.list.length, 60);
  assert.equal(ch.list[0].path, "f64.ts");
  assert.equal(ch.list.at(-1).path, "f5.ts");
});

test("新改動自動選中；點舊的一筆就停住；手動選最新後恢復自動", () => {
  const ch = createChanges();
  ch.apply([edit(A, "1.ts")]);
  assert.equal(sel(ch).path, "1.ts");
  ch.apply([edit(A, "2.ts")]);
  assert.equal(sel(ch).path, "2.ts");
  ch.select(at(ch, 1).key); // 點舊的
  assert.equal(ch.following, false);
  ch.apply([edit(B, "3.ts")]);
  assert.equal(sel(ch).path, "1.ts", "停住：新改動進來不切換");
  ch.select(at(ch, 0).key); // 手動選最新
  assert.equal(ch.following, true);
  ch.apply([edit(B, "4.ts")]);
  assert.equal(sel(ch).path, "4.ts");
});

test("停住時選中的那筆被擠出 60 筆：改選最舊的一筆，仍然停住", () => {
  const ch = createChanges({ cap: 3 });
  ch.apply([edit(A, "1.ts"), edit(A, "2.ts"), edit(A, "3.ts")]);
  ch.select(at(ch, 2).key); // 1.ts
  ch.apply([edit(A, "4.ts")]);
  assert.equal(ch.following, false);
  assert.equal(sel(ch).path, "2.ts");
});

test("snapshot 的 changes（舊到新）整批取代；同一筆 Event 不會重複", () => {
  const ch = createChanges();
  ch.apply([edit(A, "old.ts")]);
  const e1 = edit(A, "x.ts");
  const e2 = write(B, "y.ts");
  ch.load([e1, e2]);
  assert.deepEqual(paths(ch), ["y.ts", "x.ts"]);
  assert.equal(sel(ch).path, "y.ts");
  ch.apply([e2]);
  assert.equal(ch.list.length, 2);
});

test("diff 行號：hunk 從 start 起算，+／− 各自只有新／舊行號", () => {
  const rows = diffRows({ start: 41, oldLines: 3, newLines: 4, lines: [" export function f() {", "-  const a = 1;", "+  const a = 2;", "+  const b = 3;", "   return a;"] });
  assert.deepEqual(rows[0], { type: "hunk", text: "@@ -41,3 +41,4 @@" });
  assert.deepEqual(rows.slice(1), [
    { type: "ctx", old: 41, new: 41, text: "export function f() {" },
    { type: "del", old: 42, new: null, text: "  const a = 1;" },
    { type: "add", old: null, new: 42, text: "  const a = 2;" },
    { type: "add", old: null, new: 43, text: "  const b = 3;" },
    { type: "ctx", old: 43, new: 44, text: "  return a;" },
  ]);
});

test("hunk 是 null（hunkSource none）時回傳說明，不是空白", () => {
  const rows = diffRows(null, "none");
  assert.equal(rows.length, 1);
  assert.equal(rows[0].type, "note");
  assert.match(rows[0].text, /沒有 diff/);
});

test("Write 全文：從 1 起編行號；去掉結尾換行；截斷時最後附註", () => {
  assert.deepEqual(writeRows("a\nb\n", false), [{ type: "line", new: 1, text: "a" }, { type: "line", new: 2, text: "b" }]);
  const t = writeRows("a\n", true);
  assert.equal(t.at(-1).type, "note");
  assert.match(t.at(-1).text, /200 行/);
});

// ---------- 被拒或失敗的改動（#682，contracts §4） ----------

const failed = (s, id, extra = {}) => ev(s, "tool.updated", { toolUseId: id, status: "failed", durationMs: 1, error: "denied", interrupted: null, ...extra });

test("被拒或失敗的 Edit／Write 從改動紀錄拿掉；失敗先到的也不加進來", () => {
  const ch = createChanges();
  const e1 = edit(A, "a.ts");
  const e2 = edit(A, "b.ts");
  ch.apply([e1, e2]);
  ch.apply([failed(A, e1.payload.toolUseId)]);
  assert.deepEqual(paths(ch), ["b.ts"]);
  // snapshot 重載：失敗與 file.* 一起來，順序不保證
  const w = write(B, "c.ts");
  ch.load([failed(B, w.payload.toolUseId), w, e2]);
  assert.deepEqual(paths(ch), ["b.ts"]);
});

test("countHunk：只數 +／- 開頭的行，context 與空 hunk 不算（串流、改動紀錄、單 session 共用）", () => {
  assert.deepEqual(countHunk([" a", "-b", "+c", "+d"]), { added: 2, removed: 1 });
  assert.deepEqual(countHunk([]), { added: 0, removed: 0 });
  assert.deepEqual(countHunk(undefined), { added: 0, removed: 0 });
});
