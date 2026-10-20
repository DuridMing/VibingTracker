import { test } from "node:test";
import assert from "node:assert/strict";
import { layout, indexAt } from "../../web/js/vlist.js";

const rows = [{ h: 20 }, { h: 0 }, { h: 60 }, { h: 20 }];
const heightOf = (r) => r.h;

test("layout 回傳每一行的頂端位移與總高", () => {
  const top = layout(rows, heightOf);
  assert.deepEqual([...top], [0, 20, 20, 80, 100]);
});

test("indexAt 找到位移所在的行；高度 0（被篩掉）的行不會被選中", () => {
  const top = layout(rows, heightOf);
  assert.equal(indexAt(top, 0), 0);
  assert.equal(indexAt(top, 19.9), 0);
  assert.equal(indexAt(top, 20), 2);
  assert.equal(indexAt(top, 79), 2);
  assert.equal(indexAt(top, 80), 3);
});

test("超出範圍時夾在頭尾；空清單回 0", () => {
  const top = layout(rows, heightOf);
  assert.equal(indexAt(top, -50), 0);
  assert.equal(indexAt(top, 10_000), 3);
  assert.equal(indexAt(layout([], heightOf), 30), 0);
});
