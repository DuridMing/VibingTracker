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

test("DST：位移可以依時刻給，「昨天」用昨天的位移算午夜，不差一小時（#747 F-13）", () => {
  const H = 3_600_000;
  // 假時區：某個時刻之後從 UTC+1 變 UTC+2（夏令時間開始）
  const switchAt = Date.UTC(2026, 2, 29, 1, 0, 0);
  const offsetAt = (ms) => (ms >= switchAt ? 120 : 60);
  const now = Date.UTC(2026, 2, 29, 12, 0, 0); // 當地 14:00（UTC+2）
  const todayMidnight = Date.UTC(2026, 2, 28, 22, 0, 0); // 當地 3/29 00:00（那時還是 UTC+1 → UTC 前一天 23:00）
  const r = rangeFor("yesterday", now, offsetAt);
  assert.equal(r.to + 1, Date.UTC(2026, 2, 28, 23, 0, 0), "今天午夜（切換前，UTC+1）");
  assert.equal(r.from, Date.UTC(2026, 2, 27, 23, 0, 0), "昨天午夜（UTC+1）");
  assert.ok(todayMidnight < r.to + 1 + H);
});
