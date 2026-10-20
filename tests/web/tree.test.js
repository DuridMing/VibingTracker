import { test } from "node:test";
import assert from "node:assert/strict";
import { createFileTree, projectPath, gitMarks, filesUrl, sinceChanges, LAZY_THRESHOLD } from "../../web/js/tree.js";

// Event 照 contracts.md §4；file.* 的 path 相對 session 的 cwd（#672）
const A = "aaaaaaaa-0000-0000-0000-000000000001";
const B = "bbbbbbbb-0000-0000-0000-000000000002";
const ROOT = "/repo";
let seq = 0;
function ev(session, type, payload, agent = "main") {
  return { v: 1, session, seq: ++seq, type, agent, line: null, pos: null, ts_model: null, ts_exec: null, ts_seen: 1000 + seq, host: "local", payload };
}
const read = (s, path, id = `t${seq + 1}`) => ev(s, "file.read", { toolUseId: id, path });
const edit = (s, path, id = `t${seq + 1}`) => ev(s, "file.edit", { toolUseId: id, path, hunk: null, hunkSource: "none" });
const write = (s, path, isNew = true, id = `t${seq + 1}`) => ev(s, "file.write", { toolUseId: id, path, isNew, content: "", truncated: false });
const failed = (s, id, extra = {}) => ev(s, "tool.updated", { toolUseId: id, status: "failed", durationMs: 1, error: "x", interrupted: false, ...extra });

const FILES = ["README.md", "src/a.rs", "src/b.rs", "src/deep/c.rs", "web/js/main.js", "web/index.html"];
function tree(files = FILES, cwds = { [A]: ROOT, [B]: ROOT }) {
  const t = createFileTree();
  t.setFiles({ root: ROOT, tree: files, treeTruncated: false });
  t.cwdOf = (id) => cwds[id] ?? null;
  return t;
}
const names = (t) => t.visible().map((r) => `${"  ".repeat(r.depth)}${r.name}${r.dir ? "/" : ""}`);

// ---------- 路徑對應 ----------

test("projectPath：session 在根目錄、子目錄、worktree 都對應到 repo 相對路徑", () => {
  const dirs = new Set(["src", "web", "web/js"]);
  const hasDir = (d) => dirs.has(d);
  assert.equal(projectPath({ root: ROOT, cwd: ROOT, path: "src/a.rs", hasDir }), "src/a.rs");
  assert.equal(projectPath({ root: ROOT, cwd: "/repo/web", path: "js/main.js", hasDir }), "web/js/main.js", "子目錄：補上前綴");
  assert.equal(projectPath({ root: ROOT, cwd: "/repo/worktrees/feature/x", path: "src/a.rs", hasDir }), "src/a.rs", "worktree（不在樹裡的目錄）：路徑本來就相對它的根");
  assert.equal(projectPath({ root: ROOT, cwd: ROOT, path: "/repo/src/b.rs", hasDir }), "src/b.rs", "絕對路徑在根底下");
  assert.equal(projectPath({ root: ROOT, cwd: ROOT, path: "/etc/hosts", hasDir }), null, "根以外的檔不進樹");
  assert.equal(projectPath({ root: ROOT, cwd: "/other", path: "src/a.rs", hasDir }), null, "別的專案的 session");
  assert.equal(projectPath({ root: "C:/repo", cwd: "C:/repo", path: "C:/repo/src/a.rs", hasDir }), "src/a.rs", "Windows 路徑");
});

test("projectPath：collector 給了 worktree 清單就照它；被 ignore 的目錄（e2e 暫存）不算", () => {
  const hasDir = (d) => d === "web";
  const worktrees = [ROOT, "/repo/worktrees/feature/x", "/elsewhere/wt"];
  const pp = (cwd, path) => projectPath({ root: ROOT, cwd, path, hasDir, worktrees });
  assert.equal(pp("/repo/worktrees/feature/x", "src/a.rs"), "src/a.rs");
  assert.equal(pp("/repo/worktrees/feature/x/web", "js/main.js"), "web/js/main.js", "worktree 的子目錄");
  assert.equal(pp("/repo/target/tmp/e2e", "hello.txt"), null, "不是 worktree、也不在樹裡");
  assert.equal(pp("/elsewhere/wt", "src/a.rs"), "src/a.rs", "放在根以外的 worktree");
  assert.equal(pp("/repo/web", "js/main.js"), "web/js/main.js");
  assert.equal(pp(ROOT, "/repo/worktrees/feature/x/src/a.rs"), "src/a.rs", "絕對路徑在 worktree 裡");
  // 真資料：session 從主工作樹開、改 worktree 裡的檔，file.* 的 path 是相對第一次的 cwd（#672）
  assert.equal(pp(ROOT, "worktrees/feature/x/src/a.rs"), "src/a.rs", "相對路徑穿進 worktree");
  assert.equal(pp("/repo/web", "../worktrees/feature/x/web/js/main.js"), "web/js/main.js", "有 .. 的相對路徑");
});

test("setFiles 帶 worktrees：e2e 暫存目錄的 session 不進樹", () => {
  const E = "eeeeeeee-0000-0000-0000-000000000005";
  const t = tree(FILES, { [A]: "/repo/worktrees/feature/x", [E]: "/repo/target/tmp/e2e" });
  t.setFiles({ root: ROOT, tree: FILES, treeTruncated: false, worktrees: [{ path: ROOT, branch: "develop" }, { path: "/repo/worktrees/feature/x", branch: "feature/x" }] });
  t.apply([edit(A, "src/a.rs"), write(E, "hello.txt")]);
  assert.equal(t.counts("src/a.rs").e, 1);
  assert.ok(!names(t).includes("hello.txt"));
});

// ---------- 樹節點累積計數 ----------

test("讀／改／新建／失敗分開累積；Write 覆寫算改", () => {
  const t = tree();
  t.apply([read(A, "src/a.rs"), read(B, "src/a.rs"), edit(A, "src/a.rs"), write(A, "src/a.rs", false), write(B, "src/new.rs")]);
  assert.deepEqual(t.counts("src/a.rs"), { r: 2, e: 2, w: 0, x: 0, isNew: false });
  assert.deepEqual(t.counts("src/new.rs"), { r: 0, e: 0, w: 1, x: 0, isNew: true });
  assert.deepEqual(t.counts("src/b.rs"), { r: 0, e: 0, w: 0, x: 0, isNew: false });
});

test("失敗：同一個 toolUseId 的 tool.updated failed 算到那個檔，只算一次；Esc 中斷不算", () => {
  const t = tree();
  t.apply([edit(A, "src/a.rs", "e1"), failed(A, "e1"), failed(A, "e1"), edit(A, "src/b.rs", "e2"), failed(A, "e2", { interrupted: true })]);
  assert.equal(t.counts("src/a.rs").x, 1);
  assert.equal(t.counts("src/b.rs").x, 0);
  // 失敗先到、file.* 後到（同一 frame 內順序不保證）也要算到
  t.apply([failed(B, "e3"), read(B, "README.md", "e3")]);
  assert.equal(t.counts("README.md").x, 1);
});

test("同一筆 Event 在 snapshot 與 frame 都出現只算一次", () => {
  const t = tree();
  const e = edit(A, "src/a.rs");
  t.apply([e]);
  t.apply([e, read(A, "src/a.rs")]);
  assert.deepEqual(t.counts("src/a.rs"), { r: 1, e: 1, w: 0, x: 0, isNew: false });
});

test("worktree 與子目錄的 session 算到同一個節點；別的專案不算", () => {
  const C = "cccccccc-0000-0000-0000-000000000003";
  const t = tree(FILES, { [A]: "/repo/worktrees/feature/x", [B]: "/repo/web", [C]: "/elsewhere" });
  t.apply([edit(A, "web/js/main.js"), edit(B, "js/main.js"), edit(C, "web/js/main.js")]);
  assert.equal(t.counts("web/js/main.js").e, 2);
});

test("apply 回傳這批碰到的檔（閃爍用）與種類", () => {
  const t = tree();
  const hits = t.apply([read(A, "src/a.rs"), edit(B, "src/b.rs", "x1"), failed(B, "x1"), write(A, "n.txt")]);
  assert.deepEqual(hits.map((h) => [h.path, h.kind, h.session]), [["src/a.rs", "r", A], ["src/b.rs", "e", B], ["src/b.rs", "x", B], ["n.txt", "w", A]]);
});

test("正在碰它的 session：每個 session 最後碰的檔，只算還在跑的 session", () => {
  const t = tree();
  t.apply([read(A, "src/a.rs"), read(B, "src/a.rs"), edit(A, "src/b.rs")]);
  const running = new Set([A, B]);
  assert.deepEqual(t.touching("src/b.rs", running), [A]);
  assert.deepEqual(t.touching("src/a.rs", running), [B], "A 已經換到 b.rs");
  assert.deepEqual(t.touching("src/a.rs", new Set([A])), [], "B 不在跑了");
});

test("Write 新檔加節點（父目錄也長出來）", () => {
  const t = tree();
  t.apply([write(A, "src/gen/new.rs")]);
  const n = names(t);
  assert.ok(n.includes("    new.rs"), n.join("\n"));
  assert.ok(n.includes("  gen/"));
});

// ---------- 展開與懶載入 ----------

test("只展開被碰過的目錄（頂層永遠看得到）；點一下切換", () => {
  const t = tree();
  assert.deepEqual(names(t), ["src/", "web/", "README.md"], "目錄在前、都收著");
  t.apply([read(A, "src/deep/c.rs")]);
  assert.deepEqual(names(t), ["src/", "  deep/", "    c.rs", "  a.rs", "  b.rs", "web/", "README.md"]);
  t.toggle("src"); // 手動收起：之後碰到也不自動打開
  t.apply([read(A, "src/a.rs")]);
  assert.deepEqual(names(t), ["src/", "web/", "README.md"]);
  t.toggle("web");
  assert.deepEqual(names(t), ["src/", "web/", "  js/", "  index.html", "README.md"]);
});

test("樹的前綴線（├─ └─ │）", () => {
  const t = tree();
  t.toggle("src");
  const pre = t.visible().map((r) => r.pre + r.name);
  assert.deepEqual(pre, ["├─ src", "│  ├─ deep", "│  ├─ a.rs", "│  └─ b.rs", "├─ web", "└─ README.md"]);
});

test(`> ${LAZY_THRESHOLD} 檔懶載入：沒展開的目錄不建子節點`, () => {
  const big = [];
  for (let d = 0; d < 60; d++) for (let f = 0; f < 100; f++) big.push(`d${d}/sub/f${f}.txt`);
  assert.ok(big.length > LAZY_THRESHOLD);
  const t = tree(big);
  assert.equal(t.lazy, true);
  assert.equal(t.visible().length, 60);
  assert.equal(t.materialized(), 1, "只建了根");
  t.toggle("d7");
  t.toggle("d7/sub");
  assert.equal(t.visible().length, 161);
  assert.equal(t.materialized(), 3, "根、d7、d7/sub");
  const small = tree();
  assert.equal(small.lazy, false);
});

test("大樹開頁：20000 檔建樹加第一次 visible 在 50 ms 內", () => {
  const big = [];
  for (let d = 0; d < 200; d++) for (let f = 0; f < 100; f++) big.push(`pkg${d}/src/m${f}.ts`);
  // 取 3 次裡最快的：機器忙（check.sh 與其他編譯同時跑）時單次會被排程拖慢，演算法變慢則 3 次都慢（#749 T-4）
  let ms = Infinity;
  for (let i = 0; i < 3; i++) {
    const t0 = performance.now();
    const t = tree(big);
    t.apply([edit(A, "pkg3/src/m1.ts")]);
    const rows = t.visible();
    ms = Math.min(ms, performance.now() - t0);
    assert.equal(rows.length, 200 + 1 + 100);
  }
  assert.ok(ms < 50, `${ms.toFixed(1)} ms`);
});

test("setFiles 重新整理（換基準、重抓）保留累積計數與手動展開", () => {
  const t = tree();
  t.apply([edit(A, "src/a.rs")]);
  t.toggle("web");
  t.setFiles({ root: ROOT, tree: [...FILES, "src/z.rs"], treeTruncated: false });
  assert.equal(t.counts("src/a.rs").e, 1);
  assert.ok(names(t).includes("  z.rs"));
  assert.ok(names(t).includes("  js/"));
});

// ---------- git 標記（#644） ----------

const wx = { worktree: "/repo/worktrees/feature/x", branch: "feature/x" };
const wy = { worktree: "/repo/worktrees/feature/y", branch: "feature/y" };

test("gitMarks：兩個 worktree 合併成一筆；M 是琥珀、新檔紫、刪除另標；衝突照 collector", () => {
  const m = gitMarks([
    { path: "src/a.rs", conflict: true, by: [{ ...wx, status: "M" }, { ...wy, status: "M" }] },
    { path: "src/b.rs", conflict: false, by: [{ ...wx, status: "M" }, { ...wy, status: "?" }] },
    { path: "src/n.rs", conflict: false, by: [{ ...wx, status: "?" }] },
    { path: "src/m.rs", conflict: false, by: [{ ...wy, status: "A" }] },
    { path: "old.rs", conflict: false, by: [{ ...wx, status: "D" }] },
    { path: "src/r.rs", conflict: false, by: [{ ...wx, status: "R", from: "src/q.rs" }] },
  ]);
  assert.deepEqual([...m.keys()].sort(), ["old.rs", "src/a.rs", "src/b.rs", "src/m.rs", "src/n.rs", "src/r.rs"]);
  assert.equal(m.get("src/a.rs").state, "mod");
  assert.equal(m.get("src/a.rs").conflict, true);
  assert.deepEqual(m.get("src/a.rs").branches, ["feature/x", "feature/y"]);
  assert.equal(m.get("src/b.rs").state, "mod", "有一邊改既有檔就是改");
  assert.equal(m.get("src/n.rs").state, "new");
  assert.equal(m.get("src/m.rs").state, "new");
  assert.equal(m.get("old.rs").state, "del");
  assert.equal(m.get("src/r.rs").from, "src/q.rs");
});

test("樹上的 git 狀態：檔名看 git、目錄有改動就標；刪掉的檔仍列出", () => {
  const t = tree();
  t.setGit([
    { path: "src/deep/c.rs", conflict: true, by: [{ ...wx, status: "M" }, { ...wy, status: "M" }] },
    { path: "gone.rs", conflict: false, by: [{ ...wx, status: "D" }] },
  ]);
  const rows = Object.fromEntries(t.visible().map((r) => [r.path, r]));
  assert.equal(rows["src"].git, "mod", "有改動的目錄自動展開並標示");
  assert.equal(rows["src"].conflict, true);
  assert.equal(rows["src/deep/c.rs"].git, "mod");
  assert.equal(rows["src/deep/c.rs"].conflict, true);
  assert.equal(rows["gone.rs"].git, "del");
  assert.equal(rows["web"].git, null);
  t.setGit([]);
  assert.equal(Object.fromEntries(t.visible().map((r) => [r.path, r]))["gone.rs"], undefined, "換基準後刪除的檔消失");
});

test("本次開啟以來：前端累積的改動轉成 changes（Write 新檔 A、其他 M；依 session 分列）", () => {
  const t = tree();
  t.apply([read(A, "README.md"), edit(A, "src/a.rs"), write(B, "src/a.rs", false), write(B, "src/new.rs")]);
  assert.deepEqual(sinceChanges(t), [
    { path: "src/a.rs", conflict: false, by: [{ session: A, status: "M" }, { session: B, status: "M" }] },
    { path: "src/new.rs", conflict: false, by: [{ session: B, status: "A" }] },
  ]);
});

test("filesUrl：四種基準的查詢參數；本次開啟以來也要抓樹（base=main 只為了 tree）", () => {
  assert.equal(filesUrl("/repo x", { kind: "main" }), "/api/files?cwd=%2Frepo%20x&base=main");
  assert.equal(filesUrl("/repo", { kind: "session", id: A }), `/api/files?cwd=%2Frepo&base=session%3A${A}`);
  assert.equal(filesUrl("/repo", { kind: "commit", hash: "ABC1234", cwd: "/repo/wt" }), "/api/files?cwd=%2Frepo%2Fwt&base=commit%3Aabc1234");
  assert.equal(filesUrl("/repo", { kind: "commit", hash: "xyz" }), null, "hash 格式不對不送");
  assert.equal(filesUrl("/repo", { kind: "since" }), "/api/files?cwd=%2Frepo&base=main");
});

// ---------- 被拒或失敗的改動（#682，contracts §4） ----------

test("被拒或失敗的 Edit／Write 不算改：改／新建的方塊收回、本次開啟以來不列；失敗照算，Esc 中斷不算失敗", () => {
  const t = tree();
  t.apply([edit(A, "src/a.rs", "e1"), failed(A, "e1"), write(A, "src/new.rs", true, "w1"), failed(A, "w1", { interrupted: true }), edit(B, "src/a.rs", "e2")]);
  assert.deepEqual(t.counts("src/a.rs"), { r: 0, e: 1, w: 0, x: 1, isNew: false });
  assert.deepEqual(t.counts("src/new.rs"), { r: 0, e: 0, w: 0, x: 0, isNew: false });
  assert.deepEqual(sinceChanges(t), [{ path: "src/a.rs", conflict: false, by: [{ session: B, status: "M" }] }]);
  // 失敗先到、file.* 後到
  t.apply([failed(B, "e3"), edit(B, "src/b.rs", "e3")]);
  assert.deepEqual(t.counts("src/b.rs"), { r: 0, e: 0, w: 0, x: 1, isNew: false });
});

test("頁面開著幾天：去重與卡片對照的集合有上限，計數照樣正確（#766 F-5）", () => {
  const t = createFileTree();
  t.cwdOf = () => "/r";
  t.setFiles({ root: "/r", tree: ["a.txt"] });
  const evs = [];
  for (let i = 0; i < 300_000; i++) evs.push({ session: "s", seq: i + 1, type: "file.read", payload: { toolUseId: `t${i}`, path: "a.txt" } });
  t.apply(evs);
  assert.equal(t.counts("a.txt").r, 300_000);
  const z = t.sizes();
  assert.ok(z.seen <= 100_000 && z.toolPath <= 100_000, JSON.stringify(z));
  // 最近的照樣去重
  t.apply([evs.at(-1)]);
  assert.equal(t.counts("a.txt").r, 300_000);
});

test("重抓檔案樹帶上次的 treeHash（have），相同時伺服器省略 tree（#766 F-6、contracts §7）", () => {
  assert.equal(filesUrl("/repo", { kind: "main" }, "abcd1234abcd1234"), "/api/files?cwd=%2Frepo&base=main&have=abcd1234abcd1234");
  assert.equal(filesUrl("/repo", { kind: "main" }, null), "/api/files?cwd=%2Frepo&base=main");
  assert.equal(filesUrl("/repo", { kind: "commit", hash: "zz" }, "abcd"), null, "hash 格式不對照樣是 null");
});

test("使用者拒絕（PermissionDenied，denied）不算失敗：改動照樣收回，但不記「失敗」（#745 D6、contracts §4）", () => {
  const t = tree(["src/a.rs"]);
  t.apply([edit(A, "src/a.rs", "d1"), failed(A, "d1", { denied: true })]);
  assert.equal(t.counts("src/a.rs").x, 0, "被拒不是工具出錯");
  assert.equal(t.counts("src/a.rs").e, 0, "被拒的改動沒有發生，照樣收回");
});
