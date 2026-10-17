import { test } from "node:test";
import assert from "node:assert/strict";
import { thinkingText, createSessionPane, nowOf, switchTab, sessionTitle, needsReplay, snapshotLoad } from "../js/session-pane.js";

const S = "aaaaaaaa-0000-0000-0000-000000000001";
let seq = 0;
const ev = (type, payload, { pos = null, agent = "main", ts = ++seq } = {}) => ({ v: 1, session: S, seq: ++seq, type, agent, line: null, pos, ts_model: null, ts_exec: null, ts_seen: ts, host: "local", payload });
const commit = (hash, message, agent = "main", ok = true) => ev("git.op", { toolUseId: `t${hash}`, op: "commit", branch: "feature/x", hash, message, ok }, { agent });

test("snapshot：終端機只有這個 session 的行；git 列取成功的 commit，最後一筆是最新；思考條是最新一段", () => {
  const p = createSessionPane(S);
  p.load({
    staged: [],
    stream: [
      ev("assistant.thinking", { text: "先看 repo", summarized: false }, { pos: 1 }),
      ev("assistant.text", { text: "我先讀檔" }, { pos: 2 }),
      commit("aaa111", "第一筆"),
      ev("git.op", { toolUseId: "tp", op: "push", branch: "feature/x", hash: null, message: null, ok: true }),
      commit("bbb222", "失敗的", "main", false),
      ev("assistant.thinking", { text: "測試失敗，改 page 預設值", summarized: true }, { pos: 3, agent: "a1" }),
      commit("ccc333", "修好分頁", "a1"),
    ],
  });
  assert.deepEqual(p.stream.rows.map((r) => r.pos), [1, 2, 3]);
  assert.equal(p.git.commits.length, 2);
  assert.deepEqual(p.git.last, { hash: "ccc333", message: "修好分頁", agent: "a1", branch: "feature/x" });
  assert.deepEqual(p.thinking, { text: "測試失敗，改 page 預設值", agent: "a1", summarized: true });
});

test("frame 接著 snapshot 往下 append；同一個 commit 再送一次不重複算", () => {
  const p = createSessionPane(S);
  p.load({ staged: [], stream: [ev("assistant.text", { text: "a" }, { pos: 1 })] });
  const c = commit("ddd444", "加測試");
  p.apply([ev("assistant.text", { text: "b" }, { pos: 2 }), c]);
  p.apply([c]);
  assert.deepEqual(p.stream.rows.map((r) => r.pos), [1, 2]);
  assert.equal(p.git.commits.length, 1);
});

test("切回這個 tab（重新訂閱收到新的 snapshot）：整份取代，不會留下舊的或重複的行", () => {
  const p = createSessionPane(S);
  p.load({ staged: [], stream: [ev("assistant.text", { text: "a" }, { pos: 1 }), commit("e1", "x")] });
  p.apply([ev("assistant.text", { text: "b" }, { pos: 2 })]);
  // 離開期間又多了 pos 3、4 和一筆 commit；snapshot 是 collector 的最新狀態
  const snap = [1, 2, 3, 4].map((n) => ev("assistant.text", { text: `t${n}` }, { pos: n }));
  p.load({ staged: [], stream: [...snap, commit("e1", "x"), commit("e2", "y")] });
  assert.deepEqual(p.stream.rows.map((r) => r.pos), [1, 2, 3, 4]);
  assert.deepEqual(p.git.commits.map((x) => x.hash), ["e1", "e2"]);
});

test("最近一筆改動：只看這個 session 的 file.edit／file.write，最新的在前", () => {
  const p = createSessionPane(S);
  p.load({ staged: [], stream: [
    ev("file.edit", { toolUseId: "t1", path: "src/a.ts", hunk: { start: 1, oldLines: 1, newLines: 1, lines: ["-a", "+b"] }, hunkSource: "pre" }),
    ev("file.write", { toolUseId: "t2", path: "src/new.ts", isNew: true, content: "x\n", truncated: false }),
  ] });
  assert.equal(p.changes.list[0].path, "src/new.ts");
  assert.equal(p.changes.list.length, 2);
});

test("NOW：main 有動作就顯示 main；main 沒在做事時顯示執行中的 subagent；等你／完成時照 session 狀態", () => {
  const main = { agentId: "main", label: "main", status: "run", current: "$ npm test" };
  const sub = { agentId: "a1", label: "查呼叫點", status: "run", current: "read src/a.ts" };
  assert.deepEqual(nowOf({ main, active: [sub] }, { status: "run" }), { state: "run", what: "$ npm test", agent: "main" });
  assert.deepEqual(nowOf({ main: { ...main, current: null }, active: [sub] }, { status: "run" }), { state: "run", what: "read src/a.ts", agent: "查呼叫點" });
  assert.deepEqual(nowOf({ main: { ...main, current: null }, active: [] }, { status: "wait", lastPrompt: "改分頁" }), { state: "wait", what: "等你回應", agent: "main" });
  assert.deepEqual(nowOf({ main: null, active: [] }, { status: "done" }), { state: "done", what: "回合結束，等你確認", agent: "main" });
  assert.deepEqual(nowOf({ main: null, active: [] }, null), { state: "idle", what: "等待中", agent: "main" });
});

test("tab 切換：全部 → A 訂 A；A → B 先退 A 再訂 B；B → 全部 退 B；`*` 從不退訂（切回全部不必補事件）", () => {
  const calls = [];
  const client = { subscribe: (c) => calls.push(`+${c}`), unsubscribe: (c) => calls.push(`-${c}`) };
  switchTab(client, "all", "A");
  switchTab(client, "A", "B");
  switchTab(client, "B", "B");
  switchTab(client, "B", "all");
  assert.deepEqual(calls, ["+session:A", "-session:A", "+session:B", "-session:B"]);
  assert.ok(!calls.some((c) => c.includes("*")));
  // 全專案回放的分頁（#702）不是 session：不訂閱任何頻道，離開時也不退訂
  calls.length = 0;
  switchTab(client, "A", "replay");
  switchTab(client, "replay", "B");
  switchTab(client, "B", "replay");
  switchTab(client, "replay", "all");
  assert.deepEqual(calls, ["-session:A", "+session:B", "-session:B"]);
});

test("open 換到另一個 session：清空終端機、git 列、思考條、改動，之後只收新 session 的 Event", () => {
  const p = createSessionPane(S);
  p.load({ staged: [], stream: [ev("assistant.thinking", { text: "a" }, { pos: 1 }), commit("f1", "x")] });
  const T = "bbbbbbbb-0000-0000-0000-000000000002";
  p.open(T);
  assert.equal(p.session, T);
  assert.deepEqual(p.stream.rows, []);
  assert.equal(p.git.last, null);
  assert.equal(p.thinking, null);
  p.apply([ev("assistant.text", { text: "舊 session 的" }, { pos: 2 })]);
  assert.deepEqual(p.stream.rows, []);
  p.apply([{ ...ev("assistant.text", { text: "新的" }, { pos: 1 }), session: T }]);
  assert.equal(p.stream.rows.length, 1);
});

const edit = (tid, path, lines, opts = {}) => ev("file.edit", { toolUseId: tid, path, hunk: { start: 1, oldLines: 1, newLines: 1, lines }, hunkSource: "pre" }, opts);
const write = (tid, path, isNew, opts = {}) => ev("file.write", { toolUseId: tid, path, isNew, content: "x\n", truncated: false }, opts);
const called = (tid, tool, status = "running") => ev("tool.called", { toolUseId: tid, tool, input: {}, status, placement: "staged", danger: null });
const updated = (tid, status) => ev("tool.updated", { toolUseId: tid, status });

test("碰過的檔案彙總：每個檔案一列，累計改幾次、+/− 行數、是否新檔、哪些 agent；正在改的排最前面並標出來", () => {
  const p = createSessionPane(S);
  p.load({ staged: [], stream: [
    called("e1", "Edit", "done"), edit("e1", "src/a.ts", ["-x", "+y", "+z"], { agent: "main" }),
    called("w1", "Write", "done"), write("w1", "src/new.ts", true),
    called("e2", "Edit", "done"), edit("e2", "src/a.ts", ["-q"], { agent: "a1" }),
    called("e3", "Edit", "running"), edit("e3", "src/b.ts", ["+1"]),
  ] });
  const f = p.files();
  assert.deepEqual(f.map((x) => x.path), ["src/b.ts", "src/a.ts", "src/new.ts"]);
  assert.equal(f[0].running, true);
  const a = f[1];
  assert.deepEqual([a.edits, a.writes, a.added, a.removed, a.running], [2, 0, 2, 2, false]);
  assert.deepEqual(a.agents, ["main", "a1"]);
  assert.equal(f[2].isNew, true);
  // 工具結束：不再標正在改，排序改回依最近一次
  p.apply([updated("e3", "done")]);
  assert.equal(p.files().find((x) => x.path === "src/b.ts").running, false);
});

test("檔案彙總不受改動紀錄 60 筆上限影響：同一個 session 改很多次也全部算進去", () => {
  const p = createSessionPane(S);
  p.load({ staged: [], stream: Array.from({ length: 80 }, (_, i) => edit(`t${i}`, `src/f${i % 5}.ts`, ["+a"])) });
  const f = p.files();
  assert.equal(f.length, 5);
  assert.equal(f.reduce((n, x) => n + x.edits, 0), 80);
});

test("session 的滑過提示：完整 id、現在在做什麼、最近的 prompt；沒有的那行略過", () => {
  assert.equal(sessionTitle({ id: S, doing: "跑測試中", lastPrompt: "修分頁" }), `${S}\n▸ 跑測試中\n最近的 prompt：修分頁`);
  assert.equal(sessionTitle({ id: S, doing: null, lastPrompt: "修分頁" }), `${S}\n最近的 prompt：修分頁`);
  assert.equal(sessionTitle({ id: S, doing: "", lastPrompt: null }), S);
});

test("被拒或失敗的 Edit／Write 不算進碰過的檔案（#682）", () => {
  const p = createSessionPane(S);
  p.load({ staged: [], stream: [
    called("e1", "Edit", "failed"), edit("e1", "src/a.ts", ["+x"]),
    called("e2", "Edit", "done"), edit("e2", "src/b.ts", ["+y"]),
  ] });
  assert.deepEqual(p.files().map((x) => x.path), ["src/b.ts"]);
  p.apply([called("e3", "Edit"), edit("e3", "src/b.ts", ["+z"]), updated("e3", "failed")]);
  const b = p.files().find((x) => x.path === "src/b.ts");
  assert.deepEqual([b.edits, b.added], [1, 1]);
});

// ---------- 回放歷史 session（#653） ----------
// 一個已結束的 session：兩輪各有思考、文字、改檔、commit；回放時 collector 依 pos 從最新一頁往前給
function history() {
  const T = "cccccccc-0000-0000-0000-000000000003";
  const mk = (type, payload, opts) => ({ ...ev(type, payload, opts), session: T });
  const list = [
    mk("assistant.thinking", { text: "先看結構", summarized: false }, { pos: 1 }),
    mk("assistant.text", { text: "我先改 a.ts" }, { pos: 2 }),
    mk("tool.called", { toolUseId: "e1", tool: "Edit", input: {}, status: "running", placement: "staged", danger: null }),
    mk("tool.placed", { toolUseId: "e1", placement: "stream" }, { pos: 3 }),
    mk("tool.updated", { toolUseId: "e1", status: "done" }),
    mk("file.edit", { toolUseId: "e1", path: "src/a.ts", hunk: { start: 1, oldLines: 1, newLines: 2, lines: ["-a", "+b", "+c"] }, hunkSource: "pre" }),
    mk("git.op", { toolUseId: "g1", op: "commit", branch: "feature/x", hash: "aaa111", message: "第一筆", ok: true }),
    mk("assistant.thinking", { text: "第二輪：補測試", summarized: true }, { pos: 4 }),
    mk("assistant.text", { text: "新增測試檔" }, { pos: 5 }),
    mk("tool.called", { toolUseId: "w1", tool: "Write", input: {}, status: "running", placement: "staged", danger: null }),
    mk("tool.placed", { toolUseId: "w1", placement: "stream" }, { pos: 6 }),
    mk("tool.updated", { toolUseId: "w1", status: "done" }),
    mk("file.write", { toolUseId: "w1", path: "test/a.test.ts", isNew: true, content: "x\n", truncated: false }),
    mk("git.op", { toolUseId: "g2", op: "commit", branch: "feature/x", hash: "bbb222", message: "補測試", ok: true }),
    mk("assistant.text", { text: "完成" }, { pos: 7 }),
  ];
  return { T, list };
}

const view = (p) => ({
  rows: p.stream.rows.map((r) => [r.pos, r.seq ?? null, r.kind ?? r.type ?? null]),
  commits: p.git.commits.map((c) => c.hash),
  last: p.git.last?.hash ?? null,
  thinking: p.thinking,
  files: p.files().map((f) => [f.path, f.edits, f.writes, f.added, f.removed, f.isNew, f.running]),
  changes: p.changes.list.map((c) => c.key),
});

test("回放：session 已不在 live（snapshot 沒有它的串流）時要從 JSONL 讀；即時的 session 不用", () => {
  const { T, list } = history();
  assert.equal(needsReplay(T, { staged: [], stream: [] }), true);
  assert.equal(needsReplay(T, { staged: [], stream: list.filter((e) => e.pos == null) }), true, "只有狀態行、沒有任何 pos");
  assert.equal(needsReplay(T, { staged: [], stream: list }), false);
  assert.equal(needsReplay(T, { staged: [], stream: [{ ...list[0], session: S }] }), true, "別的 session 的不算");
});

test("回放：從最新一頁往前 prepend，串流、git 列、碰過的檔案、改動、思考條都和即時收到（snapshot＋frame）的一樣", () => {
  const { T, list } = history();
  const live = createSessionPane(T);
  live.load({ staged: [], stream: list.slice(0, 5) });
  live.apply(list.slice(5, 10));
  live.apply(list.slice(10));

  const replay = createSessionPane(T);
  replay.load({ staged: [], stream: [] });
  replay.prepend(list.slice(8)); // 最新一頁：pos 5 之後
  replay.prepend(list.slice(3, 8)); // 再往前
  replay.prepend(list.slice(0, 3));

  assert.deepEqual(view(replay), view(live));
  assert.deepEqual(replay.git.commits.map((c) => c.hash), ["aaa111", "bbb222"], "較舊的 commit 排在前面，最後一筆不被舊的蓋掉");
  assert.equal(replay.thinking.text, "第二輪：補測試", "思考條是最新一段，不被往前翻到的舊思考蓋掉");
  assert.equal(replay.stream.minPos().get(T), 1);
});

test("回放：舊的一頁裡工具還在 running，不能蓋掉之後已知的 done", () => {
  const { T, list } = history();
  const p = createSessionPane(T);
  p.load({ staged: [], stream: [] });
  p.prepend(list.slice(4)); // 有 e1 的 done
  p.prepend(list.slice(0, 4)); // 只有 e1 的 running
  assert.equal(p.files().find((f) => f.path === "src/a.ts").running, false);
});

test("按 ▶ 回放（#690）：snapshot 已有串流也不用它，清空後從 JSONL 由最新往前讀；沒按時照舊（有串流就用，沒有才自動回放）", () => {
  const m = { staged: [ev("tool.called", { toolUseId: "t1" })], stream: [ev("assistant.text", { text: "a" }, { pos: 1 })] };
  assert.deepEqual(snapshotLoad(S, m, { replay: true }), { load: { staged: [], stream: [] }, replay: true });
  assert.deepEqual(snapshotLoad(S, m), { load: { staged: m.staged, stream: m.stream }, replay: false });
  const gone = { staged: [], stream: [] };
  assert.deepEqual(snapshotLoad(S, gone), { load: { staged: [], stream: [] }, replay: true });
});

test("思考條的佔位文案依模式：hide 寫不提供、0 字只寫（思考）、on_demand 寫未載入（#724）", () => {
  assert.equal(thinkingText({ text: "原文" }), "原文");
  assert.equal(thinkingText({ deferred: true, chars: 227, text: "" }, "on_demand"), "（思考 227 字，遠端未載入；在串流裡點開那一行可載入）");
  assert.equal(thinkingText({ deferred: true, chars: 227, text: "" }, "hide"), "（思考 227 字，這個 collector 不對遠端提供）");
  assert.equal(thinkingText({ deferred: true, chars: 0, text: "" }, "hide"), "（思考）");
  const p = createSessionPane("s1");
  p.thinkingMode = () => "hide";
  p.load({ staged: [], stream: [{ type: "assistant.thinking", session: "s1", agent: "main", pos: 1, seq: 1, payload: { deferred: true, chars: 9, text: "" } }] });
  assert.equal(p.thinking.text, "（思考 9 字，這個 collector 不對遠端提供）");
});
