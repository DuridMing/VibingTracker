import { test } from "node:test";
import assert from "node:assert/strict";
import { parseMonitor, channelsFor, monitorUrl } from "../../web/js/monitor.js";

const ID = "617649c3-b3c4-4c43-9c4f-e7d5e77c16bd";

test("hash 解析：#monitor=<完整 session id> 才是監視器；其他 hash、空的、格式不對都不是", () => {
  assert.equal(parseMonitor(`#monitor=${ID}`), ID);
  assert.equal(parseMonitor(`#monitor=${ID.toUpperCase()}`), ID); // 大小寫不同仍是同一個 session
  assert.equal(parseMonitor(""), null);
  assert.equal(parseMonitor("#"), null);
  assert.equal(parseMonitor("#monitor="), null);
  assert.equal(parseMonitor("#other=1"), null);
  assert.equal(parseMonitor("#monitor=../../etc"), null);
  assert.equal(parseMonitor(`#monitor=${ID}&x=1`), null);
  assert.equal(parseMonitor(`#monitor=${encodeURIComponent("<script>")}`), null);
});

test("只訂單一頻道：監視器只訂 session:<id>，不訂 `*`；主視窗只訂 `*`", () => {
  assert.deepEqual(channelsFor(ID), [`session:${ID}`]);
  assert.deepEqual(channelsFor(null), ["*"]);
});

test("開監視器的網址：保留 query（例如 ?mock=12），換成 #monitor=<id>", () => {
  assert.equal(monitorUrl({ pathname: "/", search: "?mock=12", hash: "#x" }, ID), `/?mock=12#monitor=${ID}`);
  assert.equal(monitorUrl({ pathname: "/index.html", search: "", hash: "" }, ID), `/index.html#monitor=${ID}`);
});
