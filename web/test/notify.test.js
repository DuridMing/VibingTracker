import { test } from "node:test";
import assert from "node:assert/strict";
import { loadSettings, saveSettings, sinksFor, summarize, createNotifier, createLeader } from "../js/notify.js";

function fakeClock(t = 1_000_000) {
  const timers = new Map();
  let seq = 0;
  return {
    now: () => t,
    setTimeout: (fn, ms) => { const id = ++seq; timers.set(id, { at: t + ms, fn }); return id; },
    clearTimeout: (id) => timers.delete(id),
    setInterval: (fn, ms) => { const id = ++seq; timers.set(id, { at: t + ms, fn, every: ms }); return id; },
    clearInterval: (id) => timers.delete(id),
    advance(ms) {
      const end = t + ms;
      for (;;) {
        const next = [...timers.entries()].filter(([, x]) => x.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
        if (!next) break;
        const [id, x] = next;
        t = x.at;
        if (x.every) x.at += x.every; else timers.delete(id);
        x.fn();
      }
      t = end;
    },
  };
}

const mem = () => { const m = new Map(); return { getItem: (k) => m.get(k) ?? null, setItem: (k, v) => m.set(k, String(v)) }; };
const att = (id, kind, raisedAt, session = id.split("/")[0]) => ({ id, session, kind, ref: id, reason: `${kind} 原因`, raisedAt });

test("開關預設全關；存了之後讀回；壞掉或讀不到的儲存當預設", () => {
  const s = mem();
  assert.deepEqual(loadSettings(s), { toast: false, sound: false, browser: false });
  saveSettings(s, { toast: true, sound: false, browser: true });
  assert.deepEqual(loadSettings(s), { toast: true, sound: false, browser: true });
  s.setItem("coding-lab.notify", "{壞掉");
  assert.deepEqual(loadSettings(s), { toast: false, sound: false, browser: false });
  const throws = { getItem() { throw new Error("blocked"); }, setItem() { throw new Error("blocked"); } };
  assert.deepEqual(loadSettings(throws), { toast: false, sound: false, browser: false });
  assert.doesNotThrow(() => saveSettings(throws, { toast: true }));
});

test("觸發條件：toast 每個主視窗都顯示；聲音與瀏覽器通知只由 leader 發；monitor 頁一律不發", () => {
  const all = { toast: true, sound: true, browser: true };
  assert.deepEqual(sinksFor(all, { isLeader: true, isMonitor: false }), ["toast", "sound", "browser"]);
  assert.deepEqual(sinksFor(all, { isLeader: false, isMonitor: false }), ["toast"]);
  assert.deepEqual(sinksFor(all, { isLeader: true, isMonitor: true }), []);
  assert.deepEqual(sinksFor({ toast: false, sound: true, browser: false }, { isLeader: true, isMonitor: false }), ["sound"]);
  assert.deepEqual(sinksFor({ toast: false, sound: false, browser: false }, { isLeader: true, isMonitor: false }), []);
});

test("連線前已存在的項目不觸發：snapshot 裡的、以及連線之前 raised 的都不響", () => {
  const c = fakeClock();
  const out = [];
  const n = createNotifier({ ...c, emit: (x) => out.push(x) });
  n.connected();
  n.snapshot([att("s1/done/p1", "done", c.now() - 60_000)]);
  // resume 補送的 frame 裡，斷線期間 raised 的項目也不響
  n.frame([att("s2/wait/t1", "wait", c.now() - 30_000)]);
  c.advance(2000);
  assert.equal(out.length, 0);
  n.frame([att("s3/fail/t2", "fail", c.now())]);
  c.advance(1000);
  assert.equal(out.length, 1);
  assert.deepEqual(out[0].items.map((x) => x.id), ["s3/fail/t2"]);
});

test("同一個 id 再送一次（欄位更新）不重複通知", () => {
  const c = fakeClock();
  const out = [];
  const n = createNotifier({ ...c, emit: (x) => out.push(x) });
  n.connected();
  n.snapshot([]);
  n.frame([att("s1/wait/t1", "wait", c.now())]);
  c.advance(1000);
  n.frame([{ id: "s1/wait/t1", reason: "改了原因" }]);
  c.advance(1000);
  assert.equal(out.length, 1);
});

test("1 秒內多筆合併成一則；合併期間被確認或解除的不算", () => {
  const c = fakeClock();
  const out = [];
  const n = createNotifier({ ...c, emit: (x) => out.push(x) });
  n.connected();
  n.snapshot([]);
  n.frame([att("s1/done/p1", "done", c.now())]);
  c.advance(300);
  n.frame([att("s2/done/p2", "done", c.now()), att("s3/done/p3", "done", c.now())]);
  c.advance(300);
  n.frame([{ id: "s3/done/p3", deleted: true }]);
  c.advance(399);
  assert.equal(out.length, 0);
  c.advance(1);
  assert.equal(out.length, 1);
  assert.deepEqual(out[0].items.map((x) => x.id), ["s1/done/p1", "s2/done/p2"]);
  // 下一批重新計時
  n.frame([att("s4/wait/t", "wait", c.now())]);
  c.advance(1000);
  assert.equal(out.length, 2);
});

test("重新連線（重新整理或 collector 重啟）重設基準：之前看過的與 snapshot 都不補響", () => {
  const c = fakeClock();
  const out = [];
  const n = createNotifier({ ...c, emit: (x) => out.push(x) });
  n.connected();
  n.snapshot([]);
  n.frame([att("s1/done/p1", "done", c.now())]);
  n.connected(); // 合併中斷線：待發的丟掉
  c.advance(5000);
  n.snapshot([att("s1/done/p1", "done", c.now() - 5000)]);
  c.advance(5000);
  assert.equal(out.length, 0);
});

test("摘要：單筆寫出 session 與原因；同種類多筆「3 個 session 完成」；混合種類列各類數量；聲音取最高優先", () => {
  const label = (s) => s.slice(0, 2);
  const one = summarize([att("s1/wait/t", "wait", 1)], label);
  assert.equal(one.title, "等你 · s1");
  assert.equal(one.body, "wait 原因");
  assert.equal(one.kind, "wait");
  const same = summarize([att("s1/done/a", "done", 1), att("s2/done/b", "done", 1), att("s3/done/c", "done", 1)], label);
  assert.equal(same.title, "3 個 session 完成");
  assert.equal(same.kind, "done");
  const mixed = summarize([att("s1/done/a", "done", 1), att("s2/danger/b", "danger", 1), att("s2/wait/c", "wait", 1)], label);
  assert.equal(mixed.title, "2 個 session 需要注意");
  assert.equal(mixed.body, "危險 1 · 等你 1 · 完成 1");
  assert.equal(mixed.kind, "danger");
});

function bus() {
  const chans = new Set();
  return () => {
    const ch = { onmessage: null, postMessage(m) { for (const o of chans) if (o !== ch) o.onmessage?.({ data: structuredClone(m) }); }, close() { chans.delete(ch); } };
    chans.add(ch);
    return ch;
  };
}

test("leader：多個主視窗只有一個是 leader；leader 關掉後其他視窗在逾時內接手", () => {
  const c = fakeClock();
  const open = bus();
  const a = createLeader({ id: "a", channel: open(), ...c });
  const b = createLeader({ id: "b", channel: open(), ...c });
  c.advance(2000);
  assert.equal([a, b].filter((x) => x.isLeader()).length, 1);
  assert.equal(a.isLeader(), true);
  a.stop();
  c.advance(6000);
  assert.equal(b.isLeader(), true);
});

test("leader：只有自己時立刻就是 leader；沒有 BroadcastChannel 時也是", () => {
  const c = fakeClock();
  const solo = createLeader({ id: "z", channel: bus()(), ...c });
  assert.equal(solo.isLeader(), true);
  const none = createLeader({ id: "y", channel: null, ...c });
  assert.equal(none.isLeader(), true);
});

test("危險列解除時的更新（帶 clearedAt）不會再通知一次", () => {
  const c = fakeClock();
  const out = [];
  const n = createNotifier({ ...c, emit: (x) => out.push(x) });
  n.connected();
  n.snapshot([]);
  n.frame([{ ...att("s1/danger/t1", "danger", c.now()), clearedAt: null }]);
  c.advance(1000);
  n.frame([{ id: "s1/danger/t1", clearedAt: c.now() }]);
  c.advance(1000);
  assert.equal(out.length, 1);
});

test("合併中收到 snapshot（離開回放時重訂 `*`）：還在佇列裡的照發，snapshot 裡沒有的（已確認）不發（#765 C5）", () => {
  const c = fakeClock();
  const out = [];
  const n = createNotifier({ ...c, emit: (x) => out.push(x) });
  n.connected();
  n.snapshot([]);
  n.frame([att("s1/danger/t1", "danger", c.now()), att("s2/wait/t2", "wait", c.now())]);
  c.advance(200);
  // 重訂 `*` 送來的 snapshot：s1 的還在佇列，s2 的已經被確認、不在了
  n.snapshot([att("s1/danger/t1", "danger", c.now() - 200)]);
  c.advance(1000);
  assert.equal(out.length, 1, "合併中的通知不能被 snapshot 吃掉");
  assert.deepEqual(out[0].items.map((x) => x.id), ["s1/danger/t1"]);
  // 之後同一個 id 不再響
  n.frame([att("s1/danger/t1", "danger", c.now())]);
  c.advance(1000);
  assert.equal(out.length, 1);
});
