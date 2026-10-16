import { test } from "node:test";
import assert from "node:assert/strict";
import { sortAttention } from "../js/attention.js";
import { createStore, applySnapshot, applyFrame } from "../js/store.js";

const row = (id, session, kind, raisedAt) => ({ id, session, kind, ref: id, reason: "r", raisedAt });

test("依種類：危險 > 失敗 > 等你 > 完成待確認；不認得的種類排最後", () => {
  const rows = [row("d", "s1", "done", 1), row("w", "s1", "wait", 1), row("x", "s1", "conflict", 1), row("z", "s1", "danger", 1), row("f", "s1", "fail", 1), row("q", "s1", "weird", 1)];
  assert.deepEqual(sortAttention(rows, () => 0).map((r) => r.id), ["z", "f", "w", "d", "x", "q"]);
});

test("同種類依 session 分數由高到低，再依 raisedAt 由舊到新", () => {
  const score = { a: 10, b: 50, c: 10 };
  const rows = [row("a1", "a", "wait", 300), row("c1", "c", "wait", 100), row("b1", "b", "wait", 900), row("a2", "a", "wait", 200)];
  assert.deepEqual(sortAttention(rows, (s) => score[s]).map((r) => r.id), ["b1", "c1", "a2", "a1"]);
});

test("沒有分數（#645 之前）當 0，不會 NaN 亂序", () => {
  const rows = [row("b", "s2", "done", 2), row("a", "s1", "done", 1)];
  assert.deepEqual(sortAttention(rows, () => undefined).map((r) => r.id), ["a", "b"]);
});

test("已確認：collector 送 deleted 後從佇列消失", () => {
  const st = createStore();
  applySnapshot(st, { attention: [row("s1/done/p1", "s1", "done", 1), row("s2/wait/t1", "s2", "wait", 2)] });
  applyFrame(st, { rows: { attention: [{ id: "s1/done/p1", deleted: true }] } });
  assert.deepEqual(sortAttention([...st.attention.values()], () => 0).map((r) => r.id), ["s2/wait/t1"]);
});
