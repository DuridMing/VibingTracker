import { test } from "node:test";
import assert from "node:assert/strict";
import { fmtKpi, modelBreakdown } from "../js/kpi.js";
import { createStore, applySnapshot, applyFrame } from "../js/store.js";

test("費用兩位小數、思考 tokens 超過千以 k 表示、沒有值顯示 —", () => {
  assert.equal(fmtKpi("costUsd", 1.234), "$1.23");
  assert.equal(fmtKpi("thinkingTokens", 12345), "12.3k");
  assert.equal(fmtKpi("thinkingTokens", 999), "999");
  assert.equal(fmtKpi("edits", undefined), "—");
});

test("依模型分列：依指定欄位由大到小，附占比；沒有 models 回空陣列", () => {
  const models = { opus: { costUsd: 3, thinkingTokens: 100 }, haiku: { costUsd: 1, thinkingTokens: 900 } };
  assert.deepEqual(modelBreakdown(models, "costUsd").map((r) => [r.model, r.value, r.share]), [["opus", 3, 0.75], ["haiku", 1, 0.25]]);
  assert.deepEqual(modelBreakdown(models, "thinkingTokens").map((r) => r.model), ["haiku", "opus"]);
  assert.deepEqual(modelBreakdown(undefined, "costUsd"), []);
});

test("總和為 0 時占比是 0，不是 NaN", () => {
  const rows = modelBreakdown({ sonnet: { costUsd: 0, thinkingTokens: 0 } }, "costUsd");
  assert.equal(rows[0].share, 0);
});

test("frame 的 kpi.models 整個取代，消失的模型不留下", () => {
  const st = createStore();
  applySnapshot(st, { kpi: { costUsd: 2, models: { opus: { costUsd: 1, thinkingTokens: 0 }, sonnet: { costUsd: 1, thinkingTokens: 0 } } } });
  applyFrame(st, { rows: { kpi: { models: { opus: { costUsd: 2, thinkingTokens: 5 } } } } });
  assert.deepEqual(Object.keys(st.kpi.models), ["opus"]);
  assert.equal(st.kpi.costUsd, 2);
});

test("依模型分列的每列帶完整模型 id（滑過提示用，#643）；collector 沒給 ids 時退回短名", () => {
  const rows = modelBreakdown({
    "opus 5.5": { costUsd: 3, thinkingTokens: 0, ids: ["claude-opus-5-5", "claude-opus-5-5[1m]"] },
    "foo 9": { costUsd: 1, thinkingTokens: 0 },
  }, "costUsd");
  assert.deepEqual(rows.map((r) => [r.model, r.title]), [["opus 5.5", "claude-opus-5-5\nclaude-opus-5-5[1m]"], ["foo 9", "foo 9"]]);
});
