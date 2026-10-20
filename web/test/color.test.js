import { test } from "node:test";
import assert from "node:assert/strict";
import { PALETTE_SIZE, sessionColorIndex, sessionColor, shortId } from "../js/color.js";

const ID = "dba6a637-9dd5-42a5-ad28-e4f502825ab7";

test("同一個 id 永遠同色（與呼叫順序無關）", () => {
  const first = sessionColorIndex(ID);
  sessionColorIndex("166f4e4c-0000-0000-0000-000000000000");
  sessionColorIndex("4d80b0bc-0000-0000-0000-000000000000");
  assert.equal(sessionColorIndex(ID), first);
  assert.equal(sessionColor(ID), `var(--s${first})`);
});

test("色號落在調色盤範圍內", () => {
  for (let i = 0; i < 500; i++) {
    const n = sessionColorIndex(`id-${i}`);
    assert.ok(Number.isInteger(n) && n >= 0 && n < PALETTE_SIZE, `id-${i} → ${n}`);
  }
});

test("大量 id 大致平均分到每個顏色", () => {
  const counts = new Array(PALETTE_SIZE).fill(0);
  const total = PALETTE_SIZE * 400;
  for (let i = 0; i < total; i++) {
    const hex = (i * 2654435761 >>> 0).toString(16).padStart(8, "0");
    counts[sessionColorIndex(`${hex}-9dd5-42a5-ad28-${String(i).padStart(12, "0")}`)]++;
  }
  for (const c of counts) assert.ok(c > 400 * 0.7 && c < 400 * 1.3, `分佈不均：${counts}`);
});

test("shortId 取前 8 碼", () => {
  assert.equal(shortId(ID), "dba6a637");
  assert.equal(shortId("abc"), "abc");
});
