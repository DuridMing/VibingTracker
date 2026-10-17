import { test } from "node:test";
import assert from "node:assert/strict";
import { rangeFor, replayUrl, createReplay } from "../js/replay.js";

const H = 3_600_000;

test("時間範圍：最近 3 小時；今天、昨天依使用者時區的午夜（UTC+8）", () => {
  // 2026-10-17 15:00 台北 = 07:00Z
  const now = Date.UTC(2026, 9, 17, 7, 0, 0);
  assert.deepEqual(rangeFor("3h", now, 480), { from: now - 3 * H, to: now });
  const midnight = Date.UTC(2026, 9, 16, 16, 0, 0); // 10/17 00:00 台北
  assert.deepEqual(rangeFor("today", now, 480), { from: midnight, to: now });
  assert.deepEqual(rangeFor("yesterday", now, 480), { from: midnight - 24 * H, to: midnight - 1 });
});

test("API 網址：cwd 編碼、範圍、上一頁回應的 next 當游標", () => {
  assert.equal(replayUrl({ cwd: "/p q", from: 1, to: 2, limit: 50 }), "/api/projects/events?cwd=%2Fp%20q&from=1&to=2&limit=50");
  assert.equal(
    replayUrl({ cwd: "/p", from: 1, to: 2, before: { ts: 9, session: "s1", pos: 3 }, limit: 50 }),
    "/api/projects/events?cwd=%2Fp&from=1&to=2&limit=50&before=9%2Cs1%2C3",
  );
});

const ev = (session, seq, pos, ts, type = "assistant.text") => ({ v: 1, session, seq, type, agent: "main", line: null, pos, ts_model: null, ts_exec: null, ts_seen: ts, host: "local", payload: { text: `${session}-${pos}` } });

test("一頁一頁往前讀：跨 session 依 (ts_seen, session, pos)，同一個位置只出現一次；next 為 null 就讀完了", () => {
  const r = createReplay();
  r.reset({ cwd: "/p", from: 0, to: 100 });
  // 第一頁（最新）：server 依 session 分段回傳
  r.load({ events: [ev("s1", 3, 2, 30), ev("s2", 2, 1, 20), ev("s2", 4, 2, 40)], next: { ts: 20, session: "s2", pos: 1 } });
  assert.equal(r.done, false);
  assert.deepEqual(r.next, { ts: 20, session: "s2", pos: 1 });
  // 第二頁（更舊）：和第一頁重疊一筆（s2 pos 1）
  r.load({ events: [ev("s1", 2, 1, 10), ev("s2", 2, 1, 20)], next: null });
  assert.equal(r.done, true);
  assert.deepEqual(
    r.stream.rows.map((x) => `${x.session}:${x.pos}`),
    ["s1:1", "s2:1", "s1:2", "s2:2"],
  );
  assert.deepEqual(r.sessions(), ["s1", "s2"]);
});

test("重新選範圍會清空上一段", () => {
  const r = createReplay();
  r.reset({ cwd: "/p", from: 0, to: 100 });
  r.load({ events: [ev("s1", 2, 1, 10)], next: null });
  r.reset({ cwd: "/q", from: 0, to: 100 });
  assert.equal(r.stream.rows.length, 0);
  assert.equal(r.cwd, "/q");
  assert.equal(r.done, false);
  assert.equal(r.next, null);
});
