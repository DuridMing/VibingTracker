import { test } from "node:test";
import assert from "node:assert/strict";
import { installMock } from "../../web/js/mock-feed.js";

// mock 要和真的 collector 說同一套話（contracts §6），不然 ?mock 試不到單 session 的載入更多、回放、離開回放（#747 F-9）
test("mock 的 more 帶回 beforePos；null（▶ 回放）從最新往前讀；取消訂閱 `*` 後不再收 frame", async () => {
  const mock = installMock(2, { prefill: true });
  const ws = new mock.WebSocket("ws://x");
  const got = [];
  ws.onmessage = (e) => got.push(JSON.parse(e.data));
  await new Promise((ok) => (ws.onopen = ok));
  try {
    ws.send(JSON.stringify({ op: "hello", protocol: 1 }));
    ws.send(JSON.stringify({ op: "subscribe", channel: "*" }));
    const snap = got.find((m) => m.t === "snapshot");
    const sid = snap.sessions[0].id;
    const minPos = Math.min(...snap.stream.filter((e) => e.session === sid && e.pos != null).map((e) => e.pos));
    ws.send(JSON.stringify({ op: "more", session: sid, beforePos: minPos, limit: 50 }));
    ws.send(JSON.stringify({ op: "more", session: sid, beforePos: null, limit: 50 }));
    await new Promise((ok) => setTimeout(ok, 200));
    const mores = got.filter((m) => m.t === "more");
    assert.equal(mores.length, 2);
    assert.equal(mores[0].beforePos, minPos, "照請求帶回 beforePos，前端才分得出是哪個請求的回覆");
    assert.ok(mores[0].events.length > 0);
    assert.equal(mores[1].beforePos, null);
    assert.ok(mores[1].events.length > 0, "beforePos null 是從最新的往前讀");
    ws.send(JSON.stringify({ op: "unsubscribe", channel: "*" }));
    const before = got.filter((m) => m.t === "frame" && m.ch === "*").length;
    await new Promise((ok) => setTimeout(ok, 900));
    assert.equal(got.filter((m) => m.t === "frame" && m.ch === "*").length, before, "取消訂閱 `*` 之後不再收 `*` 的 frame");
  } finally {
    ws.close(); // mock 的計時器跟著連線停，不然 node 不會結束
  }
});
