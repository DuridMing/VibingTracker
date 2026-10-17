import { test } from "node:test";
import assert from "node:assert/strict";
import { sortAttention, sessionDangers, ackAllTargets } from "../js/attention.js";
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

test("危險項目工具已結束（clearedAt 有值）但還沒確認：仍在危險類，排在還沒結束的危險之後、失敗之前", () => {
  const rows = [
    { ...row("c", "s1", "danger", 1), clearedAt: 5 },
    row("f", "s1", "fail", 2),
    { ...row("a", "s2", "danger", 3), clearedAt: null },
  ];
  assert.deepEqual(sortAttention(rows, (s) => (s === "s1" ? 99 : 0)).map((r) => r.id), ["a", "c", "f"]);
});

test("collector 把危險列更新成已解除：store 淺合併帶上 clearedAt，列還在", () => {
  const st = createStore();
  applySnapshot(st, { attention: [{ ...row("s1/danger/t1", "s1", "danger", 1), clearedAt: null }] });
  applyFrame(st, { rows: { attention: [{ id: "s1/danger/t1", clearedAt: 9 }] } });
  const r = st.attention.get("s1/danger/t1");
  assert.equal(r.clearedAt, 9);
  assert.equal(r.kind, "danger");
});

test("單一 session 的危險提示：只取這個 session 的 danger 列，未結束的在前", () => {
  const rows = [
    { ...row("s1/danger/a", "s1", "danger", 1), clearedAt: 3 },
    row("s1/fail/b", "s1", "fail", 2),
    row("s2/danger/c", "s2", "danger", 3),
    { ...row("s1/danger/d", "s1", "danger", 4), clearedAt: null },
  ];
  assert.deepEqual(sessionDangers(rows, "s1").map((r) => r.id), ["s1/danger/d", "s1/danger/a"]);
  assert.deepEqual(sessionDangers(rows, "s3"), []);
});

test("全部確認（#697）：照佇列順序回傳每一列，確認中的不重送；空佇列回空陣列", () => {
  const rows = [row("d", "s1", "done", 1), row("z", "s2", "danger", 1), row("w", "s1", "wait", 1)];
  assert.deepEqual(ackAllTargets(rows, new Set(), () => 0).map((r) => r.id), ["z", "w", "d"]);
  assert.deepEqual(ackAllTargets(rows, new Set(["w"]), () => 0).map((r) => r.id), ["z", "d"]);
  assert.deepEqual(ackAllTargets([], new Set(), () => 0), []);
});
