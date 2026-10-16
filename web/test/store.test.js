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
