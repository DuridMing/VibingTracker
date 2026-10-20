import { test } from "node:test";
import assert from "node:assert/strict";
import { branchesUrl, branchDiffUrl, graphUrl, commitUrl, commitDiffUrl, changedBranches, diffLines, worktreeLabel, graphNotices } from "../../web/js/branches.js";

// contracts §7 的 /api/branches 列（#696）
const row = (branch, o = {}) => ({ branch, worktree: `/repo/wt/${branch}`, integration: false, ahead: 0, behind: 0, dirty: 0, changed: 0, sessions: [], shared: [], last: null, ...o });

test("網址：cwd、worktree、path 都編碼", () => {
  assert.equal(branchesUrl("/a b"), "/api/branches?cwd=%2Fa%20b");
  assert.equal(branchDiffUrl("/r", "/r/wt/x", "src/a b.js"), "/api/branch-diff?cwd=%2Fr&worktree=%2Fr%2Fwt%2Fx&path=src%2Fa%20b.js");
  assert.equal(graphUrl("/r"), "/api/graph?cwd=%2Fr");
  assert.equal(commitUrl("/r", { hash: "abc1" }), "/api/commit?cwd=%2Fr&hash=abc1");
  assert.equal(commitUrl("/r", { worktree: "/r/wt/x" }), "/api/commit?cwd=%2Fr&worktree=%2Fr%2Fwt%2Fx");
  assert.equal(commitDiffUrl("/r", { hash: "abc1" }, "a b"), "/api/commit-diff?cwd=%2Fr&hash=abc1&path=a%20b");
});

test("標題計數：有改動（已 commit 或未 commit）的分支，不算整合分支", () => {
  const rows = [row("develop", { integration: true, dirty: 3 }), row("x", { changed: 1 }), row("y", { dirty: 1 }), row("z")];
  assert.equal(changedBranches({ branches: rows }), 2);
  assert.equal(changedBranches(null), 0);
});

test("diff 轉成行：檔頭、hunk、新增、刪除、不變", () => {
  const d = "diff --git a/a.txt b/a.txt\nindex 1..2 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n-base\n+y\n keep\n";
  assert.deepEqual(diffLines(d).map((l) => l.kind), ["meta", "meta", "meta", "meta", "hunk", "del", "add", "ctx"]);
  assert.deepEqual(diffLines(d).filter((l) => l.kind === "add").map((l) => l.text), ["y"]);
  assert.deepEqual(diffLines(""), []);
});

test("worktree 顯示：主工作樹寫「主工作樹」，其他相對 repo 根", () => {
  assert.equal(worktreeLabel("/repo", "/repo"), "主工作樹");
  assert.equal(worktreeLabel("/repo", "/repo/worktrees/feature/x"), "worktrees/feature/x");
  assert.equal(worktreeLabel("/repo", "/elsewhere/x"), "/elsewhere/x");
});

import { layoutGraph, withWip } from "../../web/js/branches.js";

const c = (hash, ...parents) => ({ hash, parents });
// 線段寫成 "x1,y1>x2,y2"（x 是欄，y 是 0／0.5／1：列的上緣、圓點、下緣）
const segs = (row) => row.segments.map((s) => `${s.x1},${s.y1}>${s.x2},${s.y2}`).sort();

test("lane：直線的歷史都在第 0 欄，上下接起來", () => {
  const g = layoutGraph([c("a", "b"), c("b", "c"), c("c")]);
  assert.deepEqual(g.rows.map((r) => r.col), [0, 0, 0]);
  assert.deepEqual(segs(g.rows[0]), ["0,0.5>0,1"]);
  assert.deepEqual(segs(g.rows[1]), ["0,0.5>0,1", "0,0>0,0.5"]);
  assert.deepEqual(segs(g.rows[2]), ["0,0>0,0.5"]);
  assert.equal(g.width, 1);
});

test("lane：兩條分支從同一個 commit 分出，到那個 commit 時收回來", () => {
  const g = layoutGraph([c("x", "b"), c("y", "b"), c("b", "c"), c("c")]);
  assert.deepEqual(g.rows.map((r) => r.col), [0, 1, 0, 0]);
  assert.deepEqual(segs(g.rows[1]), ["0,0>0,1", "1,0.5>1,1"], "y 開新的一欄，x 的線直穿");
  assert.deepEqual(segs(g.rows[2]), ["0,0.5>0,1", "0,0>0,0.5", "1,0>0,0.5"], "y 的線斜著收進 b");
  assert.equal(g.width, 2);
});

test("lane：merge commit 的第二個 parent 開新的一欄", () => {
  const g = layoutGraph([c("m", "a", "b"), c("a", "r"), c("b", "r"), c("r")]);
  assert.deepEqual(g.rows.map((r) => r.col), [0, 0, 1, 0]);
  assert.deepEqual(segs(g.rows[0]), ["0,0.5>0,1", "0,0.5>1,1"]);
  assert.notEqual(g.rows[0].segments.find((s) => s.x2 === 1).lane, g.rows[0].segments.find((s) => s.x2 === 0).lane, "兩條線不同顏色");
});

test("lane：同一條線一路同色；收掉的欄之後可以重用", () => {
  const g = layoutGraph([c("x", "b"), c("y", "b"), c("b", "c"), c("z", "c"), c("c")]);
  assert.equal(g.rows[0].lane, g.rows[2].lane, "x 與 b 在同一條線");
  assert.equal(g.rows[3].col, 1, "y 的欄收掉之後，z 用回第 1 欄");
});

test("未 commit 列：放在那個 worktree 的 HEAD 正上方，parent 是 HEAD；乾淨的 worktree 不加", () => {
  const commits = [c("x", "b"), c("b", "c"), c("c")];
  const branches = [
    { branch: "feature/x", worktree: "/r/wt/x", head: "x", dirty: 2, sessions: [] },
    { branch: "dev", worktree: "/r", head: "b", dirty: 1, sessions: ["s1"] },
    { branch: "clean", worktree: "/r/wt/c", head: "c", dirty: 0, sessions: [] },
  ];
  const rows = withWip(commits, branches);
  assert.deepEqual(rows.map((r) => r.hash), ["wip:/r/wt/x", "x", "wip:/r", "b", "c"]);
  assert.deepEqual(rows[2].parents, ["b"]);
  assert.equal(rows[2].wip.branch, "dev");
});

// #709：commit 圖沒截斷時底下不能多一列（直接把 null 交給 replaceChildren 會顯示文字「null」）
test("截斷提示：沒截斷回空陣列，截斷才有一列", () => {
  const commits = [{ hash: "a" }, { hash: "b" }, { hash: "c" }];
  assert.deepEqual(graphNotices({ commits, truncated: false }), []);
  assert.deepEqual(graphNotices({ commits, truncated: true }), ["只列最近 3 個 commit"]);
});
