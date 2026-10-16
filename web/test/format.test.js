import { test } from "node:test";
import assert from "node:assert/strict";
import { formatRow } from "../js/format.js";

const S = "aaaaaaaa-0000-0000-0000-000000000001";
const base = { key: "k", session: S, agent: "main", pos: 1, ts: 1, seq: 1, v: 0, expanded: false, delayed: false };
const tool = (card) => ({ ...base, type: "tool", card: { toolUseId: "t", status: "done", input: {}, danger: null, fallback: false, ...card } });

test("前綴：session id 前 8 碼（session 色、滑過看完整 id）與 agent", () => {
  const f = formatRow({ ...base, type: "text", text: "hi" });
  assert.match(f.html, /class="ss" style="color:var\(--s\d+\)" title="aaaaaaaa-0000-0000-0000-000000000001">\[aaaaaaaa\]/);
  assert.match(f.html, /\[main\]/);
  const bg = formatRow({ ...base, agent: "bg", type: "text", text: "x" });
  assert.match(bg.html, /\[背景\]/);
  const sub = formatRow({ ...base, agent: "a04575936d2b1e470", type: "text", text: "x" }, { agentName: () => "找 .md 檔" });
  assert.match(sub.html, /\[找 \.md 檔\]/);
});

test("所有文字都經過跳脫", () => {
  const f = formatRow(tool({ tool: "Bash", input: { command: "echo <script>alert(1)</script> & \"q\"" } }));
  assert.doesNotMatch(f.html, /<script>/);
  assert.match(f.html, /&lt;script&gt;/);
  assert.match(f.html, /&amp; &quot;q&quot;/);
});

test("危險指令整行紅底並附原因", () => {
  const f = formatRow(tool({ tool: "Bash", input: { command: "git push --force" }, status: "running", danger: { rule: "force-push", level: "danger", why: "強制推送" } }));
  assert.match(f.cls, /\bdz\b/);
  assert.match(f.html, /⚠ 強制推送/);
});

test("降級卡片標「無 transcript」、延遲的思考標「延遲」、摘要版標「摘要」", () => {
  assert.match(formatRow(tool({ tool: "Read", input: { file_path: "a.ts" }, fallback: true })).html, /無 transcript/);
  const th = formatRow({ ...base, type: "thinking", text: "想", summarized: true, delayed: true });
  assert.match(th.html, /延遲/);
  assert.match(th.html, /摘要/);
  assert.doesNotMatch(formatRow({ ...base, type: "thinking", text: "想", summarized: false }).html, /延遲|摘要/);
});

test("工具狀態：排隊中、執行中、完成耗時、失敗 exit code", () => {
  const cmd = { tool: "Bash", input: { command: "make" } };
  assert.match(formatRow(tool({ ...cmd, status: "pending" })).html, /排隊中/);
  assert.match(formatRow(tool({ ...cmd, status: "running" })).html, /class="run"/);
  assert.match(formatRow(tool({ ...cmd, status: "done", durationMs: 1234 })).html, /✓ 1\.2s/);
  assert.match(formatRow(tool({ ...cmd, status: "failed", result: { ok: false, exitCode: 2, output: "" } })).html, /✗ exit 2/);
});

test("Edit 顯示增減行數；Write 顯示新檔案行數；Task 顯示派出的 agent", () => {
  assert.match(formatRow(tool({ tool: "Edit", input: { file_path: "c.py" }, edit: { path: "c.py", added: 2, removed: 1 } })).html, /edit.*c\.py.*\+2.*−1/);
  assert.match(formatRow(tool({ tool: "Write", input: { file_path: "n.ts" }, write: { path: "n.ts", isNew: true, lines: 12 } })).html, /write.*n\.ts.*新檔案 · 12 行/);
  assert.match(formatRow(tool({ tool: "Task", input: { description: "找 .md" }, agentName: "找 .md 檔" })).html, /派出 <b>找 \.md 檔<\/b>/);
});

test("可展開：思考、多行訊息、有輸出的指令；短的讀檔不可展開", () => {
  assert.equal(formatRow({ ...base, type: "thinking", text: "一\n二" }).detail, "一\n二");
  assert.equal(formatRow({ ...base, type: "text", text: "短" }).detail, null);
  assert.equal(formatRow(tool({ tool: "Bash", input: { command: "ls" }, result: { ok: true, exitCode: 0, output: "a\nb" } })).detail, "$ ls\na\nb");
  assert.equal(formatRow(tool({ tool: "Read", input: { file_path: "a.ts" } })).detail, null);
});

test("狀態行：prompt、等你、完成", () => {
  assert.match(formatRow({ ...base, type: "status", status: "prompt", text: "修 bug" }).html, /▶.*修 bug/);
  assert.match(formatRow({ ...base, type: "status", status: "wait", text: "" }).html, /⏸ 等你回答/);
  assert.match(formatRow({ ...base, type: "status", status: "done", text: "" }).html, /✔ 完成/);
});

test("只有 tool.placed、缺伴隨 Event 的卡片顯示「內容不可用」", () => {
  const f = formatRow(tool({ tool: null, status: "pending" }));
  assert.match(f.html, /內容不可用/);
  assert.equal(f.detail, null);
});

test("背景 agent 的工具一律 fallback 進串流（contracts.md §2），不標「無 transcript」", () => {
  const bg = formatRow({ ...tool({ tool: "Bash", input: { command: "git log -1" }, fallback: true }), agent: "bg" });
  assert.doesNotMatch(bg.html, /無 transcript/);
  assert.match(bg.html, /\[背景\]/);
  // main 的降級卡片照常標示
  assert.match(formatRow(tool({ tool: "Bash", input: { command: "ls" }, fallback: true })).html, /無 transcript/);
});

test("狀態行：已中斷；背景工作完成不當成使用者的 prompt", () => {
  assert.match(formatRow({ ...base, type: "status", status: "idle", text: "" }).html, /⏹ 已中斷/);
  const task = formatRow({ ...base, type: "status", status: "prompt", source: "task", text: "<task-notification>build done</task-notification>" });
  assert.match(task.html, /背景工作完成/);
  assert.doesNotMatch(task.html, /▶/);
  assert.equal(task.detail, "<task-notification>build done</task-notification>");
  assert.match(formatRow({ ...base, type: "status", status: "prompt", source: "user", text: "修 bug" }).html, /▶.*修 bug/);
});

test("subagent 結束的狀態行：← 名稱 完成／✗ 失敗／⏹ 已停止；最後的訊息放展開", () => {
  const row = (result, text = "找到 3 個呼叫點") => ({ ...base, key: `k${result}`, agent: "a1", type: "status", status: "agent", agentId: "a1", result, text });
  const name = { agentName: (s, a) => (a === "a1" ? "查呼叫點" : null) };
  const d = formatRow(row("done"), name);
  assert.match(d.html, /← <b>查呼叫點<\/b> 完成/);
  assert.equal(d.detail, "找到 3 個呼叫點");
  assert.match(formatRow(row("fail"), name).html, /✗ <b>查呼叫點<\/b> 失敗/);
  assert.match(formatRow(row("killed"), name).html, /⏹ <b>查呼叫點<\/b> 已停止/);
  assert.equal(formatRow(row("done", ""), name).detail, null);
  // 名稱跳脫
  assert.doesNotMatch(formatRow(row("done"), { agentName: () => "<x>" }).html, /<x>/);
});

test("session 狀態行：resume、compact、clear 各有自己的文字（#651）", () => {
  const base = { key: "k", session: "s", agent: "main", type: "status", status: "session", text: "" };
  assert.match(formatRow({ ...base, source: "resume" }).html, /resume/);
  assert.match(formatRow({ ...base, source: "compact" }).html, /compact/);
  assert.match(formatRow({ ...base, source: "clear" }).html, /clear/);
});
