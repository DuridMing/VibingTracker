import { test } from "node:test";
import assert from "node:assert/strict";
import { norm, same, relUnder, under } from "../js/paths.js";

test("正規化：反斜線換成 /、去掉結尾的 /、磁碟機代號大寫（contracts §4 路徑正規化，#748 L-11）", () => {
  assert.equal(norm("e:\\proj\\x\\"), "E:/proj/x");
  assert.equal(norm("/repo/"), "/repo");
  assert.equal(norm(null), "");
});

test("比對：Windows 路徑不分大小寫，其他分（VS Code 終端機給 e:\\，Claude Code 給 E:\\）", () => {
  assert.equal(same(norm("e:\\Proj"), norm("E:\\proj")), true);
  assert.equal(same("/Repo", "/repo"), false);
  assert.equal(relUnder(norm("e:\\proj\\worktrees\\x"), norm("E:\\Proj")), "worktrees/x");
  assert.equal(relUnder("/repo", "/repo"), "");
  assert.equal(relUnder("/repo2", "/repo"), null, "前綴相同但不是底下");
  assert.equal(under("/repo/a/b", "/repo"), true);
  assert.equal(under("/other", "/repo"), false);
});
