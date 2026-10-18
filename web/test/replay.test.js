import { test } from "node:test";
import assert from "node:assert/strict";
import { rangeFor } from "../js/replay.js";

const H = 3_600_000;

test("時間範圍：最近 3 小時；今天、昨天依使用者時區的午夜（UTC+8）", () => {
  // 2026-10-17 15:00 台北 = 07:00Z
  const now = Date.UTC(2026, 9, 17, 7, 0, 0);
  assert.deepEqual(rangeFor("3h", now, 480), { from: now - 3 * H, to: now });
  const midnight = Date.UTC(2026, 9, 16, 16, 0, 0); // 10/17 00:00 台北
  assert.deepEqual(rangeFor("today", now, 480), { from: midnight, to: now });
  assert.deepEqual(rangeFor("yesterday", now, 480), { from: midnight - 24 * H, to: midnight - 1 });
});
