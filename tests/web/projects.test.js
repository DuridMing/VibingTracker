import { test } from "node:test";
import assert from "node:assert/strict";
import { projectColors, projectLabels, projectOf, sessionFilterFor, groupByProject, projectTreeRows } from "../../web/js/projects.js";
import { PALETTE_SIZE } from "../../web/js/color.js";

const ROOTS = ["/home/u/alpha", "/home/u/beta", "/srv/gamma"];

test("專案色（#669）：同一組專案每次分到同樣的色；不同專案不撞色（≤ 色票數時）", () => {
  const a = projectColors(ROOTS);
  const b = projectColors([...ROOTS].reverse());
  assert.deepEqual([...a.entries()].sort(), [...b.entries()].sort(), "順序不影響");
  assert.equal(new Set(a.values()).size, ROOTS.length, "三個專案三種色");
  for (const i of a.values()) assert.ok(Number.isInteger(i) && i >= 0 && i < PALETTE_SIZE);
  const many = projectColors(Array.from({ length: PALETTE_SIZE }, (_, i) => `/p/${i}`));
  assert.equal(new Set(many.values()).size, PALETTE_SIZE, "剛好填滿色票時每色各一");
});

test("專案色：新增一個專案不會改到既有專案的色（除非撞到才讓新的一個往後找）", () => {
  const before = projectColors(ROOTS);
  const after = projectColors([...ROOTS, "/home/u/delta"]);
  for (const [cwd, i] of before) assert.equal(after.get(cwd), i, cwd);
});

test("專案名稱：用最後一段；最後一段重複時往前多帶一段", () => {
  assert.deepEqual([...projectLabels(ROOTS).values()], ["alpha", "beta", "gamma"]);
  const dup = projectLabels(["/a/repo", "/b/repo", "/c/other"]);
  assert.equal(dup.get("/a/repo"), "a/repo");
  assert.equal(dup.get("/b/repo"), "b/repo");
  assert.equal(dup.get("/c/other"), "other");
  assert.equal(projectLabels(["C:/x"]).get("C:/x"), "x", "Windows 路徑");
});

test("projectOf：session 的 cwd 歸到哪個專案根（worktree、子目錄歸上層）；不在任何專案底下回 null", () => {
  assert.equal(projectOf("/home/u/alpha/worktrees/feature/x", ROOTS), "/home/u/alpha");
  assert.equal(projectOf("/home/u/beta", ROOTS), "/home/u/beta");
  assert.equal(projectOf("/home/u/alphabet", ROOTS), null, "只是前綴像、不是子目錄");
  assert.equal(projectOf(null, ROOTS), null);
});

test("專案篩選：全部（null）不篩；單一專案只留那個專案的 session，不知道 cwd 的 session 也留著", () => {
  const cwdOf = (id) => ({ a: "/home/u/alpha", a2: "/home/u/alpha/web", b: "/home/u/beta" })[id] ?? null;
  assert.equal(sessionFilterFor(null, ROOTS, cwdOf), null);
  const f = sessionFilterFor("/home/u/alpha", ROOTS, cwdOf);
  assert.equal(f("a"), true);
  assert.equal(f("a2"), true);
  assert.equal(f("b"), false);
  assert.equal(f("unknown"), true, "還沒拿到 cwd 的 session 不能被藏起來");
});

test("依專案分組：依第一次出現的順序分組，組內順序不變；不屬於任何專案的放最後一組（cwd null）", () => {
  const items = [{ id: 1, cwd: "/home/u/beta" }, { id: 2, cwd: "/home/u/alpha/web" }, { id: 3, cwd: "/home/u/beta/x" }, { id: 4, cwd: "/nowhere" }];
  const g = groupByProject(items, (it) => projectOf(it.cwd, ROOTS));
  assert.deepEqual(g.map((x) => [x.cwd, x.items.map((i) => i.id)]), [["/home/u/beta", [1, 3]], ["/home/u/alpha", [2]], [null, [4]]]);
});

test("檔案樹的專案列（全部專案時）：每個專案一個標題列接著它的列；收起的專案只留標題；只有一個專案時不加標題", () => {
  const rows = (n, cwd) => Array.from({ length: n }, (_, i) => ({ path: `f${i}`, cwd }));
  const one = projectTreeRows([{ cwd: "/a", rows: rows(2, "/a") }], new Set());
  assert.deepEqual(one.map((r) => r.hdr ? `#${r.cwd}` : r.path), ["f0", "f1"]);
  const two = projectTreeRows([{ cwd: "/a", rows: rows(2, "/a") }, { cwd: "/b", rows: rows(1, "/b") }], new Set(["/b"]));
  assert.deepEqual(two.map((r) => r.hdr ? `#${r.cwd}` : r.path), ["#/a", "f0", "f1", "#/b"]);
  assert.equal(two[0].open, true);
  assert.equal(two[3].open, false);
});

test("專案篩選：同一個 session 只算一次歸屬，cwd 還不知道的不快取（#757 E-F1）", async () => {
  const { sessionFilterFor } = await import("../../web/js/projects.js");
  let lookups = 0;
  const cwds = new Map([["a", "/p/w"], ["b", "/q"]]);
  const f = sessionFilterFor("/p", ["/p", "/q"], (id) => { lookups++; return cwds.get(id); });
  for (let i = 0; i < 1000; i++) f("a");
  assert.equal(lookups, 1, "同一個 session 的歸屬要記住");
  assert.equal(f("b"), false);
  assert.equal(f("c"), true, "cwd 不知道的不藏");
  cwds.set("c", "/q");
  assert.equal(f("c"), false, "cwd 知道了之後重新判斷");
});
