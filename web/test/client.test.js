import { test } from "node:test";
import assert from "node:assert/strict";
import { createClient, PROTOCOL } from "../js/client.js";

// 假的 WebSocket 與計時器：測試不 sleep，由測試推動時間。
function harness() {
  const sockets = [];
  class FakeSocket {
    constructor(url) {
      this.url = url;
      this.sent = [];
      this.readyState = 0;
      sockets.push(this);
    }
    send(s) { this.sent.push(JSON.parse(s)); }
    close() { this.readyState = 3; this.onclose?.({}); }
    // 測試用
    open() { this.readyState = 1; this.onopen?.({}); }
    recv(obj) { this.onmessage?.({ data: typeof obj === "string" ? obj : JSON.stringify(obj) }); }
    drop() { this.readyState = 3; this.onclose?.({}); }
  }
  const timers = [];
  const clock = {
    setTimeout(fn, ms) { const t = { fn, ms }; timers.push(t); return t; },
    clearTimeout(t) { const i = timers.indexOf(t); if (i >= 0) timers.splice(i, 1); },
    fire() { const t = timers.shift(); assert.ok(t, "沒有排程的計時器"); t.fn(); return t.ms; },
  };
  const got = { welcome: [], snapshot: [], frame: [], more: [], error: [], status: [], incompatible: [] };
  const handlers = {
    welcome: (m) => got.welcome.push(m),
    snapshot: (m) => got.snapshot.push(m),
    frame: (m) => got.frame.push(m),
    more: (m) => got.more.push(m),
    onError: (e) => got.error.push(e),
    onStatus: (s) => got.status.push(s),
    onIncompatible: (m) => got.incompatible.push(m),
  };
  const client = createClient({ url: "ws://h/ws", WebSocket: FakeSocket, ...clock, handlers });
  const at = (i) => { assert.ok(sockets.at(i), `沒有第 ${i} 條連線`); return sockets.at(i); };
  return { sockets, at, timers, clock, got, client };
}

const welcome = { t: "welcome", protocol: PROTOCOL, host: "local", version: "0.1.0", remote: false, thinking: "full", scope: "operate" };

test("連上後先送 hello，再補送已訂閱的頻道", () => {
  const { sockets, at, client } = harness();
  client.subscribe("*");
  client.subscribe("session:abc");
  client.connect();
  const ws = at(0);
  assert.equal(ws.url, "ws://h/ws");
  assert.deepEqual(ws.sent, []);
  ws.open();
  assert.deepEqual(ws.sent, [
    { op: "hello", protocol: PROTOCOL },
    { op: "subscribe", channel: "*" },
    { op: "subscribe", channel: "session:abc" },
  ]);
});

test("已連線時 subscribe／unsubscribe 立刻送出；重複訂閱不重送", () => {
  const { sockets, at, client } = harness();
  client.connect();
  at(0).open();
  client.subscribe("*");
  client.subscribe("*");
  client.unsubscribe("*");
  assert.deepEqual(at(0).sent.slice(1), [
    { op: "subscribe", channel: "*" },
    { op: "unsubscribe", channel: "*" },
  ]);
});

test("依 t 分派訊息；不認得的 t 忽略；壞 JSON 回報錯誤但不丟例外", () => {
  const { sockets, at, client, got } = harness();
  client.connect();
  const ws = at(0);
  ws.open();
  ws.recv(welcome);
  ws.recv({ t: "snapshot", ch: "*", frame: 3, sessions: [] });
  ws.recv({ t: "frame", frame: 4, ch: "*", events: [], rows: {} });
  ws.recv({ t: "more", session: "abc", events: [] });
  ws.recv({ t: "from-the-future", x: 1 });
  ws.recv("{not json");
  assert.equal(got.welcome.length, 1);
  assert.equal(got.snapshot[0].frame, 3);
  assert.equal(got.frame[0].frame, 4);
  assert.equal(got.more.length, 1);
  assert.equal(got.error.length, 1);
});

test("斷線後重連，hello 帶上次收到的 frame 編號", () => {
  const { sockets, at, clock, client } = harness();
  client.connect();
  at(0).open();
  at(0).recv(welcome);
  at(0).recv({ t: "frame", frame: 41, ch: "*", events: [], rows: {} });
  at(0).recv({ t: "frame", frame: 42, ch: "*", events: [], rows: {} });
  at(0).drop();
  clock.fire();
  at(1).open();
  assert.deepEqual(at(1).sent[0], { op: "hello", protocol: PROTOCOL, resume: 42 });
});

test("重連間隔指數退避到上限，收到 welcome 後歸零", () => {
  const { sockets, at, clock, client } = harness();
  client.connect();
  const waits = [];
  for (let i = 0; i < 7; i++) {
    at(-1).drop();
    waits.push(clock.fire());
  }
  assert.deepEqual(waits, [500, 1000, 2000, 4000, 8000, 10000, 10000]);
  at(-1).open();
  at(-1).recv(welcome);
  at(-1).drop();
  assert.equal(clock.fire(), 500);
});

test("協定版本不符：通知、關閉、不再重連", () => {
  const { sockets, at, timers, client, got } = harness();
  client.connect();
  at(0).open();
  at(0).recv({ ...welcome, protocol: PROTOCOL + 1 });
  assert.equal(got.incompatible.length, 1);
  assert.equal(got.welcome.length, 0);
  assert.equal(timers.length, 0);
  assert.equal(sockets.length, 1);
});

test("使用者主動 close 不重連", () => {
  const { sockets, at, timers, client } = harness();
  client.connect();
  at(0).open();
  client.close();
  assert.equal(timers.length, 0);
});

test("狀態依序回報 connecting → open → closed → connecting", () => {
  const { sockets, at, clock, client, got } = harness();
  client.connect();
  at(0).open();
  at(0).recv(welcome);
  at(0).drop();
  clock.fire();
  assert.deepEqual(got.status, ["connecting", "open", "closed", "connecting"]);
});

test("ack 與 more 照契約欄位送出；未連線時不送也不丟例外", () => {
  const { sockets, at, client } = harness();
  client.ack({ session: "s1", kind: "done", ref: "t1" });
  client.connect();
  at(0).open();
  client.ack({ session: "s1", kind: "done", ref: "t1" });
  client.more({ session: "s1", beforePos: 200, limit: 900 });
  assert.deepEqual(at(0).sent.slice(1), [
    { op: "ack", session: "s1", kind: "done", ref: "t1" },
    { op: "more", session: "s1", beforePos: 200, limit: 500 },
  ]);
});
