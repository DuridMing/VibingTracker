import { test } from "node:test";
import assert from "node:assert/strict";
import { SPEEDS, timelineUrl, createTimeline } from "../js/timeline.js";

const frame = (ts) => ({ t: "frame", ch: "*", frame: ts, ts, events: [], rows: {} });

test("API 參數：cwd 編碼、at、until、時區；接續下一段不要 snapshot", () => {
  assert.equal(timelineUrl({ cwd: "/p q", at: 10, until: 20, offset: 480 }), "/api/projects/replay?cwd=%2Fp%20q&at=10&until=20&offset=480&limit=2000");
  assert.equal(timelineUrl({ cwd: "/p", at: 10, until: 20, offset: -300, snapshot: false, limit: 50 }), "/api/projects/replay?cwd=%2Fp&at=10&until=20&offset=-300&limit=50&snapshot=0");
});

test("速度選項：1×、10×、60×、600×", () => {
  assert.deepEqual(SPEEDS, [1, 10, 60, 600]);
});

test("開始：停在範圍開頭、還沒播放；回放中擋下即時的 `*`", () => {
  const tl = createTimeline();
  assert.equal(tl.blocksLive(), false, "還沒進回放：即時照常");
  tl.start({ cwd: "/p", from: 1000, to: 9000 });
  assert.equal(tl.active, true);
  assert.equal(tl.t, 1000);
  assert.equal(tl.playing, false);
  assert.equal(tl.blocksLive(), true, "回放中收到即時的 snapshot／frame 不能改回放畫面");
  tl.stop();
  assert.equal(tl.blocksLive(), false);
});

test("載入：舊的請求回來要丟掉（拖動之後才回來的上一段）", () => {
  const tl = createTimeline();
  tl.start({ cwd: "/p", from: 1000, to: 9000 });
  const first = tl.seek(1000);
  const second = tl.seek(5000);
  assert.equal(tl.loaded(first, { snapshot: { t: "snapshot" }, frames: [frame(1100)], next: null }), null, "過期的回應不套用");
  assert.deepEqual(tl.queue, []);
  const snap = tl.loaded(second, { snapshot: { t: "snapshot", x: 1 }, frames: [frame(5100)], next: null });
  assert.deepEqual(snap, { t: "snapshot", x: 1 });
  assert.equal(tl.t, 5000);
  assert.deepEqual(tl.queue.map((f) => f.ts), [5100]);
});

test("拖動（seek）：清掉舊段的 frame、時間跳到拖到的位置並夾在範圍內", () => {
  const tl = createTimeline();
  tl.start({ cwd: "/p", from: 1000, to: 9000 });
  tl.loaded(tl.seek(1000), { snapshot: {}, frames: [frame(1500), frame(2500)], next: 2500 });
  tl.seek(99_999);
  assert.deepEqual(tl.queue, []);
  assert.equal(tl.next, null);
  assert.equal(tl.t, 9000, "不超過範圍結尾");
  tl.seek(0);
  assert.equal(tl.t, 1000, "不早於範圍開頭");
});

test("播放：依經過時間 × 速度前進，只交出 ts ≤ 目前時刻的 frame（依序）", () => {
  const tl = createTimeline();
  tl.start({ cwd: "/p", from: 1000, to: 100_000 });
  tl.loaded(tl.seek(1000), { snapshot: {}, frames: [frame(1500), frame(2000), frame(9000)], next: null });
  assert.deepEqual(tl.tick(100), [], "沒在播放時不前進");
  assert.equal(tl.t, 1000);
  tl.play(10);
  assert.deepEqual(tl.tick(50).map((f) => f.ts), [1500], "50 ms × 10 = 0.5 秒 → 1500（剛好等於也交出）");
  assert.deepEqual(tl.tick(50).map((f) => f.ts), [2000]);
  assert.equal(tl.t, 2000, "1000 + 100 × 10");
  assert.deepEqual(tl.tick(600).map((f) => f.ts), [], "8000 還沒到 9000");
  assert.deepEqual(tl.tick(100).map((f) => f.ts), [9000], "再 1 秒 → 9000");
  assert.deepEqual(tl.queue, []);
});

test("播到範圍結尾就停，不超過", () => {
  const tl = createTimeline();
  tl.start({ cwd: "/p", from: 1000, to: 2000 });
  tl.loaded(tl.seek(1000), { snapshot: {}, frames: [], next: null });
  tl.play(600);
  tl.tick(1000);
  assert.equal(tl.t, 2000);
  assert.equal(tl.playing, false);
  assert.equal(tl.ended, true);
});

test("快用完時要下一段：有 next、沒在載入、佇列裡剩下的不多", () => {
  const tl = createTimeline();
  tl.start({ cwd: "/p", from: 1000, to: 100_000 });
  const req = tl.seek(1000);
  tl.loaded(req, { snapshot: {}, frames: [frame(1500)], next: 1500 });
  assert.equal(tl.needsMore(), true);
  const more = tl.loadingMore();
  assert.equal(tl.needsMore(), false, "已經在要了");
  tl.appended(more, { frames: [frame(1600), frame(1700)], next: null });
  assert.deepEqual(tl.queue.map((f) => f.ts), [1500, 1600, 1700]);
  assert.equal(tl.needsMore(), false, "沒有下一段了");
  tl.seek(5000);
  assert.equal(tl.appended(more, { frames: [frame(1800)], next: null }), false, "拖動之後才回來的接續段丟掉");
});
