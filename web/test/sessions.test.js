import { test } from "node:test";
import assert from "node:assert/strict";
import { mergeSessions, projectRoots, pickProject, canReplay } from "../js/sessions.js";

const row = (id, extra = {}) => ({ id, cwd: "/repo", status: "run", startedAt: 1000, model: null, ...extra });

test("Sessions tab：WS 的列蓋過 /api/sessions 的同一列；已結束的從 API 補上；新到舊", () => {
  const live = [row("a", { startedAt: 3000, status: "wait", model: "opus" })];
  const fetched = [row("a", { startedAt: 3000, status: "run", model: "sonnet" }), row("old", { startedAt: 1000, status: "ended", model: "haiku" }), row("b", { startedAt: 2000, status: "ended" })];
  const out = mergeSessions(live, fetched);
  assert.deepEqual(out.map((s) => s.id), ["a", "b", "old"]);
  assert.equal(out[0].status, "wait");
  assert.equal(out[0].model, "opus");
  assert.equal(out[2].model, "haiku");
});

test("Sessions tab：WS 的列沒有模型時，用 API 查到的（SQLite 從 usage 補的，#673）", () => {
  const out = mergeSessions([row("a", { model: null })], [row("a", { model: "sonnet" })]);
  assert.equal(out[0].model, "sonnet");
});

test("專案：cwd 在另一個 session 的 cwd 底下（worktree、子目錄）就歸到上層", () => {
  const sessions = [row("a", { cwd: "/repo" }), row("b", { cwd: "/repo/worktrees/feature/x" }), row("c", { cwd: "/repo/web" }), row("d", { cwd: "/other" }), row("e", { cwd: "/repo2" })];
  assert.deepEqual(projectRoots(sessions), [
    { cwd: "/other", sessions: ["d"], live: 1 },
    { cwd: "/repo", sessions: ["a", "b", "c"], live: 3 },
    { cwd: "/repo2", sessions: ["e"], live: 1 },
  ]);
});

test("專案：只有 worktree 的 session 時，它自己就是專案；已結束的不算 live", () => {
  const sessions = [row("b", { cwd: "/repo/worktrees/feature/x", status: "ended" }), row("c", { cwd: "/repo/worktrees/feature/y" })];
  assert.deepEqual(projectRoots(sessions).map((p) => [p.cwd, p.live]), [["/repo/worktrees/feature/x", 0], ["/repo/worktrees/feature/y", 1]]);
});

test("預設專案：使用者選過且還在就用它，否則選 live 最多的", () => {
  const roots = [{ cwd: "/a", sessions: ["1"], live: 1 }, { cwd: "/b", sessions: ["2", "3"], live: 2 }];
  assert.equal(pickProject(roots, null), "/b");
  assert.equal(pickProject(roots, "/a"), "/a");
  assert.equal(pickProject(roots, "/gone"), "/b");
  assert.equal(pickProject([], "/a"), null);
});

test("▶ 回放按鍵只給已結束的 session（#690）", () => {
  assert.equal(canReplay(row("a", { status: "ended" })), true);
  for (const status of ["run", "wait", "done", "fail", "danger", "idle"]) assert.equal(canReplay(row("a", { status })), false, status);
  assert.equal(canReplay(null), false);
});

import { fitTabs } from "../js/sessions.js";

test("tab 列（#704）：全部放得下時不收、沒有「+N」", () => {
  assert.deepEqual(fitTabs([100, 100, 100], 300, 40, -1), { shown: [0, 1, 2], hidden: [] });
});

test("tab 列：放不下時留出「+N」的寬度，後面的收起來", () => {
  assert.deepEqual(fitTabs([100, 100, 100, 100], 330, 40, -1), { shown: [0, 1], hidden: [2, 3] });
});

test("tab 列：選中的被收起來時換到列上，擠掉最後一個看得到的（太寬就再多擠一個）", () => {
  assert.deepEqual(fitTabs([100, 100, 100, 100], 330, 40, 3), { shown: [0, 3], hidden: [1, 2] });
  assert.deepEqual(fitTabs([100, 100, 100, 250], 330, 40, 3), { shown: [3], hidden: [0, 1, 2] });
  assert.deepEqual(fitTabs([100, 100, 100, 100], 330, 40, 1), { shown: [0, 1], hidden: [2, 3] }, "選中的本來就看得到：不動");
});
