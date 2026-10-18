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

test("session 狀態行：resume、compact 各有自己的文字（#651；clear 是新 session，不標，#688）", () => {
  const base = { key: "k", session: "s", agent: "main", type: "status", status: "session", text: "" };
  assert.match(formatRow({ ...base, source: "resume" }).html, /resume/);
  assert.match(formatRow({ ...base, source: "compact" }).html, /compact/);
});

test("工具輸出（#654）：超過 20 行只顯示前 20 行並註明總行數", () => {
  const out = Array.from({ length: 30 }, (_, i) => `line ${i + 1}`).join("\n");
  const d = formatRow(tool({ tool: "Read", input: { file_path: "a.md" }, result: { ok: true, output: out } })).detail;
  assert.match(d, /line 20/);
  assert.doesNotMatch(d, /line 21/);
  assert.match(d, /共 30 行，只顯示前 20 行/);
  const short = formatRow(tool({ tool: "Read", input: { file_path: "a.md" }, result: { ok: true, output: "a\nb" } })).detail;
  assert.equal(short, "a\nb");
});

test("工具輸出（#654）：遠端被剝除的敏感輸出說明原因", () => {
  const d = formatRow(tool({ tool: "Read", input: { file_path: ".env" }, result: { ok: true, output: "", redacted: true } })).detail;
  assert.match(d, /敏感輸出/);
});

import { formatRow as fmtRow } from "../js/format.js";
test("串流前綴（#669）：全部專案時每行先標專案色標；沒給 projectTag 就和以前一樣", () => {
  const row = { key: "k", session: "aaaaaaaa-1", agent: "main", type: "text", text: "hi", v: 0, expanded: false, delayed: false };
  const plain = fmtRow(row, {});
  assert.ok(!plain.html.includes('class="pj"'));
  const tagged = fmtRow(row, { projectTag: () => ({ label: "alpha", color: "var(--s3)", cwd: "/home/u/alpha" }) });
  assert.ok(tagged.html.startsWith('<span class="pj"'), tagged.html);
  assert.ok(tagged.html.includes("alpha") && tagged.html.includes("var(--s3)"));
  assert.ok(tagged.html.includes('title="/home/u/alpha"'));
});

test("遠端未載入的思考：佔位帶字數，展開前的 detail 是提示；載入中與失敗各自顯示（#668）", () => {
  const f = formatRow({ ...base, type: "thinking", text: "", deferred: true, chars: 1240 });
  assert.match(f.html, /1,240 字/);
  assert.match(f.html, /點開載入/);
  assert.equal(f.detail, "點一下載入思考內容");
  assert.equal(formatRow({ ...base, type: "thinking", text: "", deferred: true, loading: true }).detail, "載入中…");
  assert.equal(formatRow({ ...base, type: "thinking", text: "", deferred: true, loadError: "不給看" }).detail, "不給看");
  // 載入後就是一般的思考列
  const loaded = formatRow({ ...base, type: "thinking", text: "一\n二", deferred: false });
  assert.doesNotMatch(loaded.html, /點開載入/);
  assert.equal(loaded.detail, "一\n二");
});

test("串流前綴（#663）：hub 時每行最前面標 [host]；沒給 hostTag 就沒有", () => {
  const row = { key: "k", session: "aaaaaaaa-1", agent: "main", type: "text", text: "hi", v: 0, expanded: false, delayed: false };
  assert.ok(!fmtRow(row, {}).html.includes('class="hst"'));
  const tagged = fmtRow(row, { hostTag: () => "build-01", projectTag: () => ({ label: "p", color: "var(--s1)", cwd: "/p" }) });
  assert.ok(tagged.html.startsWith('<span class="hst" title="build-01">[build-01]</span><span class="pj"'), tagged.html);
  assert.equal(fmtRow(row, { hostTag: () => null }).html.includes("hst"), false);
});

test("hide 模式與 0 字的思考佔位不寫「點開載入」、不可展開（#721）", () => {
  const hide = formatRow({ ...base, type: "thinking", text: "", deferred: true, chars: 227 }, { thinkingMode: "hide" });
  assert.match(hide.html, /227 字，這個 collector 不對遠端提供/);
  assert.doesNotMatch(hide.html, /點開載入/);
  assert.equal(hide.detail, null);
  const zero = formatRow({ ...base, type: "thinking", text: "", deferred: true, chars: 0 }, { thinkingMode: "on_demand" });
  assert.match(zero.html, /（思考）/);
  assert.doesNotMatch(zero.html, /點開載入|0 字/);
  assert.equal(zero.detail, null);
  const od = formatRow({ ...base, type: "thinking", text: "", deferred: true, chars: 5 }, { thinkingMode: "on_demand" });
  assert.match(od.html, /點開載入/);
  assert.equal(od.detail, "點一下載入思考內容");
});

test("Write／Edit 的計數一律當數字：hub 原樣轉送上游的 JSON，不能讓字串變成 HTML（F-8）", () => {
  const evil = "<img src=x onerror=alert(1)>";
  const w = formatRow(tool({ tool: "Write", input: { file_path: "n.ts" }, write: { path: "n.ts", isNew: false, lines: evil } })).html;
  assert.doesNotMatch(w, /<img/);
  assert.match(w, /（0 行）/);
  const e = formatRow(tool({ tool: "Edit", input: { file_path: "a.ts" }, edit: { added: evil, removed: evil } })).html;
  assert.doesNotMatch(e, /<img/);
});
