import { test } from "node:test";
import assert from "node:assert/strict";
import { agentView, agentWho } from "../js/agents.js";
import { createStore, applySnapshot, applyFrame } from "../js/store.js";

const S = "s1";
const ag = (agentId, extra = {}) => ({ id: `${S}/${agentId}`, session: S, agentId, agentType: null, name: agentId, task: null, status: "run", current: null, toolCount: 0, parentToolUseId: null, spawnedAt: 0, finishedAt: null, ...extra });

test("順序：main、背景，再來是執行中與出事的 subagent（依派出時間）；已完成的收成一個計數", () => {
  const rows = [
    ag("a2", { agentType: "Explore", name: "找測試", spawnedAt: 20 }),
    ag("a1", { agentType: "Explore", name: "查呼叫點", spawnedAt: 10, status: "done", finishedAt: 30 }),
    ag("bg", { name: "背景" }),
    ag("main", { current: "$ npm test" }),
    ag("a3", { agentType: "Plan", name: "規劃", spawnedAt: 5, status: "fail" }),
    ag("a4", { agentType: "Explore", name: "掃 md", spawnedAt: 1, status: "done", finishedAt: 9 }),
  ];
  const v = agentView(rows, S);
  assert.equal(v.main.agentId, "main");
  assert.equal(v.bg.agentId, "bg");
  assert.deepEqual(v.active.map((a) => a.agentId), ["a3", "a2"]);
  assert.deepEqual(v.done.map((a) => a.agentId), ["a1", "a4"]); // 最近完成的在前
  assert.equal(v.running, 1);
});

test("顯示名稱：name → agentType → agent id 前 8 碼；換行與多餘空白壓成一行", () => {
  const v = agentView([ag("main"), ag("x1", { name: "  找\n呼叫點   與\t測試 ", spawnedAt: 1 }), ag("x2", { name: "", agentType: "Explore", spawnedAt: 2 }), ag("abcdef1234567890", { name: null, spawnedAt: 3 })], S);
  assert.deepEqual(v.active.map((a) => a.label), ["找 呼叫點 與 測試", "Explore", "abcdef12"]);
  assert.equal(v.main.label, "main");
});

test("模型：main 顯示 session 主模型；subagent 只在與 main 不同時標出", () => {
  const v = agentView([ag("main"), ag("x1", { model: "haiku" }), ag("x2", { model: "opus" }), ag("x3")], S, { sessionModel: "opus" });
  assert.equal(v.main.model, "opus");
  assert.deepEqual(v.active.map((a) => a.model), ["haiku", null, null]);
});

test("沒有 main 列（還沒收到）時 main 是 null，不會丟例外；別的 session 的列不混進來", () => {
  const v = agentView([ag("x1"), { ...ag("y"), id: "s2/y", session: "s2" }], S);
  assert.equal(v.main, null);
  assert.deepEqual(v.active.map((a) => a.agentId), ["x1"]);
});

test("即時反映：frame 派出 subagent 後出現在 active，結束後移到 done，session 結束（deleted）後全部消失", () => {
  const st = createStore();
  applySnapshot(st, { agents: [ag("main")] });
  applyFrame(st, { rows: { agents: [ag("x1", { name: "查呼叫點", spawnedAt: 5 })] } });
  assert.deepEqual(agentView([...st.agents.values()], S).active.map((a) => a.agentId), ["x1"]);
  applyFrame(st, { rows: { agents: [{ id: `${S}/x1`, status: "done", finishedAt: 9, current: null }] } });
  let v = agentView([...st.agents.values()], S);
  assert.deepEqual(v.active, []);
  assert.deepEqual(v.done.map((a) => a.agentId), ["x1"]);
  applyFrame(st, { rows: { agents: [{ id: `${S}/main`, deleted: true }, { id: `${S}/x1`, deleted: true }] } });
  v = agentView([...st.agents.values()], S);
  assert.equal(v.main, null);
  assert.deepEqual(v.done, []);
});

test("剛完成的 subagent 先留在標籤列 5 秒（recent），之後才收進 done；沒給 now 時不分", () => {
  const rows = [ag("main"), ag("x1", { status: "done", finishedAt: 10_000 }), ag("x2", { status: "done", finishedAt: 3_000 })];
  const v = agentView(rows, S, { now: 12_000 });
  assert.deepEqual((v.recent ?? []).map((a) => a.agentId), ["x1"]);
  assert.deepEqual(v.done.map((a) => a.agentId), ["x2"]);
  const later = agentView(rows, S, { now: 15_001 });
  assert.deepEqual(later.recent, []);
  assert.deepEqual(later.done.map((a) => a.agentId), ["x1", "x2"]);
  const noClock = agentView(rows, S);
  assert.deepEqual(noClock.recent, []);
  assert.equal(noClock.done.length, 2);
});

test("agentWho：main、背景固定名稱；subagent 用 agentName，查不到時用 id 前 8 碼", () => {
  const names = (session, id) => (session === S && id === "a1" ? "查呼叫點" : null);
  assert.equal(agentWho(S, "main", names), "main");
  assert.equal(agentWho(S, "bg", names), "背景");
  assert.equal(agentWho(S, "a1", names), "查呼叫點");
  assert.equal(agentWho(S, "abcdef0123456789", names), "abcdef01");
  assert.equal(agentWho(S, "abcdef0123456789"), "abcdef01");
});
