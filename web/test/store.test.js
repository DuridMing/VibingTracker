import { test } from "node:test";
import assert from "node:assert/strict";
import { createStore, applySnapshot, applyFrame } from "../js/store.js";

const snap = {
  t: "snapshot",
  ch: "*",
  frame: 10,
  sessions: [
    { id: "a", status: "run", prompt: "p1" },
    { id: "b", status: "wait", prompt: "p2" },
  ],
  kpi: { live: 2, commits: 0 },
};

test("snapshot 整批取代 sessions 與 kpi，保留 server 給的順序", () => {
  const st = createStore();
  st.sessions.set("old", { id: "old" });
  applySnapshot(st, snap);
  assert.deepEqual([...st.sessions.keys()], ["a", "b"]);
  assert.deepEqual(st.kpi, { live: 2, commits: 0 });
});

test("frame 的 rows 以 id upsert：只改有送的欄位，新的 id 加在最後", () => {
  const st = createStore();
  applySnapshot(st, snap);
  applyFrame(st, {
    t: "frame",
    frame: 11,
    ch: "*",
    events: [],
    rows: { sessions: [{ id: "a", status: "done" }, { id: "c", status: "run", prompt: "p3" }], kpi: { commits: 1 } },
  });
  assert.deepEqual([...st.sessions.keys()], ["a", "b", "c"]);
  assert.deepEqual(st.sessions.get("a"), { id: "a", status: "done", prompt: "p1" });
  assert.deepEqual(st.kpi, { live: 2, commits: 1 });
});

test("沒有 rows 或 rows 缺欄位的 frame 不動彙總", () => {
  const st = createStore();
  applySnapshot(st, snap);
  applyFrame(st, { t: "frame", frame: 12, ch: "*", events: [] });
  applyFrame(st, { t: "frame", frame: 13, ch: "*", events: [], rows: {} });
  assert.equal(st.sessions.size, 2);
  assert.deepEqual(st.kpi, { live: 2, commits: 0 });
});

test("沒有 id 的 row 丟掉，不會產生 undefined 的 session", () => {
  const st = createStore();
  applyFrame(st, { t: "frame", frame: 1, ch: "*", events: [], rows: { sessions: [{ status: "run" }] } });
  assert.equal(st.sessions.size, 0);
});

test("deleted 的列從彙總移除", () => {
  const st = createStore();
  applySnapshot(st, snap);
  applyFrame(st, { t: "frame", frame: 11, ch: "*", events: [], rows: { sessions: [{ id: "a", deleted: true }] } });
  assert.deepEqual([...st.sessions.keys()], ["b"]);
});

test("agents 與 attention 也照 snapshot 取代、frame upsert／刪除", () => {
  const st = createStore();
  applySnapshot(st, {
    ...snap,
    agents: [{ id: "a/main", session: "a", agentId: "main", status: "run" }],
    attention: [{ id: "b/wait/t1", session: "b", kind: "wait", ref: "t1", raisedAt: 1 }],
  });
  assert.ok(st.agents instanceof Map && st.attention instanceof Map, "store 要有 agents 與 attention");
  assert.deepEqual([...st.agents.keys()], ["a/main"]);
  assert.deepEqual([...st.attention.keys()], ["b/wait/t1"]);
  applyFrame(st, {
    t: "frame", frame: 11, ch: "*", events: [],
    rows: {
      agents: [{ id: "a/main", status: "idle" }, { id: "a/bg", session: "a", agentId: "bg", status: "run" }],
      attention: [{ id: "b/wait/t1", deleted: true }],
    },
  });
  assert.equal(st.agents.get("a/main").status, "idle");
  assert.equal(st.agents.get("a/main").agentId, "main");
  assert.ok(st.agents.has("a/bg"));
  assert.equal(st.attention.size, 0);
});

test("offline 列只標既有的 session，不為沒看過的 id 建空列（#723）", () => {
  const st = createStore();
  applySnapshot(st, snap);
  applyFrame(st, { t: "frame", frame: 11, ch: "*", rows: { sessions: [
    { id: "a", host: "linux", offline: true },
    { id: "ghost", host: "linux", offline: true },
  ] } });
  assert.equal(st.sessions.get("a").offline, true);
  assert.equal(st.sessions.get("a").status, "run", "其他欄位保留");
  assert.equal(st.sessions.has("ghost"), false, "沒看過的 id 不能變成只有 offline 的空列");
  // 重連後的 snapshot 整批取代：a 不再 offline
  applySnapshot(st, { ...snap, sessions: [{ id: "a", status: "run", prompt: "p1" }] });
  assert.equal(st.sessions.get("a").offline, undefined);
  assert.deepEqual([...st.sessions.keys()], ["a"]);
});
