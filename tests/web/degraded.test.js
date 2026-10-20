import { test } from "node:test";
import assert from "node:assert/strict";
import { degradedNotice } from "../../web/js/degraded.js";

const s = (id, degraded = []) => ({ id, status: "run", degraded });

test("降級標語（#652）：沒有 session 降級就不顯示", () => {
  assert.equal(degradedNotice([s("a"), s("b")]), null);
  assert.equal(degradedNotice([{ id: "c" }]), null, "舊 collector 沒有這個欄位也不出錯");
});

test("降級標語：列出降級的 session（id 前 8 碼）與「文字與思考不可用，只顯示工具事件」", () => {
  const n = degradedNotice([s("aaaaaaaa1111"), s("bbbbbbbb2222", ["main"]), s("cccccccc3333", ["agent-x"])]);
  assert.deepEqual(n.sessions, ["bbbbbbbb2222", "cccccccc3333"]);
  assert.match(n.text, /bbbbbbbb、cccccccc/);
  assert.match(n.text, /文字與思考不可用，只顯示工具事件/);
});

test("降級標語：只有 subagent 的檔降級時註明是 subagent", () => {
  const n = degradedNotice([s("dddddddd4444", ["agent-x"])]);
  assert.match(n.text, /subagent/);
});

test("降級標語：已結束的 session 不算", () => {
  assert.equal(degradedNotice([{ id: "e", status: "ended", degraded: ["main"] }]), null);
});
