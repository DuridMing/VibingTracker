import { createClient } from "./client.js";
import { createStore, applySnapshot, applyFrame } from "./store.js";
import { sessionColor, shortId } from "./color.js";
import { installMock } from "./mock-feed.js";
import { createStream } from "./stream.js";
import { createStreamView } from "./stream-view.js";
import { formatRow } from "./format.js";

const $ = (sel) => document.querySelector(sel);

function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") el.className = v;
    else if (k === "style") el.style.cssText = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v);
  }
  for (const c of children) if (c != null) el.append(c);
  return el;
}

const STATUS_TEXT = { run: "執行中", wait: "等你", done: "待確認", fail: "失敗", danger: "危險", ended: "結束" };
const CONN_TEXT = { connecting: "連線中…", open: "已連線", closed: "已斷線，重新連線中…", incompatible: "collector 版本不相容，請重新整理或更新" };

const store = createStore();
const ui = { tab: "all", conn: "connecting", welcome: null };

// ---------- render ----------
// 每幀都會被叫到；內容沒變的面板不重建 DOM（避免整頁重新排版拖慢串流）
const lastSig = new Map();
function changed(name, sig) {
  if (lastSig.get(name) === sig) return false;
  lastSig.set(name, sig);
  return true;
}

function fmtKpi(k, v) {
  if (v == null) return "—";
  if (k === "costUsd") return `$${v.toFixed(2)}`;
  if (k === "thinkingTokens") return v >= 1000 ? `${(v / 1000).toFixed(1)}k` : String(v);
  return String(v);
}

function renderConn() {
  const el = $("#conn");
  el.className = `conn ${ui.conn}`;
  const text = $("#connText");
  text.textContent = CONN_TEXT[ui.conn];
  if (ui.conn === "open" && ui.welcome) {
    text.append(" ", h("b", {}, `${ui.welcome.host} · v${ui.welcome.version}`));
  }
  document.body.classList.toggle("stale", ui.conn !== "open");
}

function renderKpis() {
  if (!changed("kpi", JSON.stringify(store.kpi))) return;
  for (const b of document.querySelectorAll("#kpis b[data-k]")) b.textContent = fmtKpi(b.dataset.k, store.kpi[b.dataset.k]);
}

function liveSessions() {
  return [...store.sessions.values()].filter((s) => s.status !== "ended");
}

function sessTag(s, cls = "id") {
  return h("span", { class: cls, style: `color:${sessionColor(s.id)}`, title: s.id }, shortId(s.id));
}

function renderTabs() {
  const box = $("#stabs");
  const live = liveSessions();
  if (!changed("tabs", ui.tab + "|" + live.map((s) => `${s.id}:${s.status}:${s.lastPrompt ?? ""}`).join("|"))) return;
  box.replaceChildren(
    h("button", { class: `stab${ui.tab === "all" ? " on" : ""}`, onclick: () => selectTab("all") }, "全部串流 ", h("small", {}, String(live.length))),
    ...live.map((s) =>
      h("button", { class: `stab ${s.status}${ui.tab === s.id ? " on" : ""}`, title: `${s.id}\n${s.lastPrompt ?? ""}`, onclick: () => selectTab(s.id) },
        h("span", { class: "sw", style: `background:${sessionColor(s.id)}` }),
        h("span", { class: "dot" }),
        sessTag(s))),
  );
}

function renderSessFilters() {
  const live = liveSessions();
  if (!changed("fSess", live.map((s) => `${s.id}:${stream.hiddenSessions.has(s.id)}`).join("|"))) return;
  $("#fSess").replaceChildren(
    ...(live.length
      ? live.map((s) => {
          const off = stream.hiddenSessions.has(s.id);
          return h("button", { class: `fchip${off ? " off" : ""}`, title: `${s.id}\n點一下${off ? "顯示" : "隱藏"}這個 session`, "aria-pressed": String(!off), onclick: () => toggleSession(s.id) },
            h("i", { style: `background:${sessionColor(s.id)}` }), h("span", { class: "mono" }, shortId(s.id)));
        })
      : [h("span", { class: "none" }, "無")]),
  );
}

const KIND_LABEL = [["th", "思考"], ["tx", "訊息"], ["rd", "讀"], ["ed", "改"], ["wr", "寫／派工"], ["cmd", "指令與輸出"], ["st", "狀態"]];

function renderKindFilters() {
  if (!changed("fKind", [...stream.hiddenKinds].sort().join("|"))) return;
  $("#fKind").replaceChildren(
    ...KIND_LABEL.map(([k, label]) => {
      const off = stream.hiddenKinds.has(k);
      return h("button", { class: `fchip${off ? " off" : ""}`, "aria-pressed": String(!off), onclick: () => toggleKind(k) }, label);
    }),
  );
}

// 暫存區：hook 已到、transcript 還沒到的卡片（PLAN「方案 C」），不進串流，只在這裡看
function renderLive() {
  $("#liveN").textContent = String(stream.staged.size);
  const list = $("#liveList");
  if (list.hidden || !changed("live", [...stream.staged.values()].slice(-30).map((c) => `${c.toolUseId}:${c.status}`).join("|"))) return;
  const cards = [...stream.staged.values()].slice(-30);
  list.replaceChildren(
    ...(cards.length
      ? cards.map((c) => {
          const f = formatRow({ key: c.toolUseId, session: c.session, agent: c.agent, type: "tool", card: c, v: 0, expanded: false, delayed: false }, { agentName });
          const el = h("div", { class: f.cls });
          el.innerHTML = `<div class="hd">${f.html}</div>`;
          return el;
        })
      : [h("div", { class: "none" }, "沒有進行中的動作")]),
  );
}

function renderSessions() {
  const all = [...store.sessions.values()];
  if (!changed("sess", ui.tab + "|" + all.map((s) => `${s.id}:${s.status}:${s.firstPrompt}:${s.branch}:${s.commits}:${s.dangers}`).join("|"))) return;
  $("#sessCount").textContent = all.length ? String(all.length) : "";
  $("#sess").replaceChildren(
    ...(all.length
      ? all.map((s) =>
          h("button", { class: ui.tab === s.id ? "sel" : "", onclick: () => selectTab(s.id) },
            sessTag(s),
            h("span", {}, h("span", { class: "pr", title: s.firstPrompt ?? "" }, s.firstPrompt ?? "—"), h("small", {}, `⎇ ${s.branch ?? "—"} · commits ${s.commits ?? 0}${s.dangers ? ` · ⚠ ${s.dangers}` : ""}`)),
            h("span", { class: `r ${s.status}` }, STATUS_TEXT[s.status] ?? s.status ?? "")))
      : [h("div", { class: "none" }, "尚無 session")]),
  );
}

const AGENT_STATUS_TEXT = { run: "執行中", idle: "閒置", done: "完成", fail: "失敗", danger: "注意" };

// 每個 session 一列：main、subagent 各一個 chip；bg 只在執行中出現、淡色（contracts.md §6）。
function agentChip(a) {
  const isBg = a.agentId === "bg";
  return h("span", { class: `agc ${a.status}${isBg ? " bg" : ""}`, title: [a.name, a.task, AGENT_STATUS_TEXT[a.status]].filter(Boolean).join("\n") },
    h("i"), h("b", {}, a.name ?? a.agentId),
    !isBg && a.current ? h("span", { class: "cur" }, a.current) : null);
}

function renderAgents() {
  const bySession = new Map();
  for (const a of store.agents.values()) {
    if (!bySession.has(a.session)) bySession.set(a.session, []);
    bySession.get(a.session).push(a);
  }
  const live = liveSessions();
  if (!changed("agents", live.map((s) => s.id).join("|") + "#" + [...store.agents.values()].map((a) => `${a.id}:${a.status}:${a.name}:${a.current ?? ""}`).join("|"))) return;
  $("#agall").replaceChildren(
    ...(live.length
      ? live.map((s) => {
          const list = bySession.get(s.id) ?? [];
          const rank = (a) => (a.agentId === "main" ? 0 : a.agentId === "bg" ? 1 : 2);
          list.sort((x, y) => rank(x) - rank(y) || (x.spawnedAt ?? 0) - (y.spawnedAt ?? 0));
          return h("div", { class: "agrow" }, sessTag(s, "ss"), h("span", { class: "ags" }, ...(list.length ? list.map(agentChip) : [h("span", { class: "none" }, "—")])));
        })
      : [h("div", { class: "none" }, "尚無 session")]),
  );
}

let rafPending = false;
let renderOnce = false;
function render() {
  if (bench.isolate && renderOnce) return; // 量測用：只畫第一次，之後其他面板不動
  if (rafPending) return;
  rafPending = true;
  requestAnimationFrame(() => {
    rafPending = false;
    renderOnce = stream.rows.length >= 3000;
    renderConn();
    renderKpis();
    renderTabs();
    renderSessFilters();
    renderKindFilters();
    renderSessions();
    renderAgents();
    renderLive();
  });
}

// ---------- 合流串流 ----------
const stream = createStream();
const agentName = (session, agentId) => store.agents.get(`${session}/${agentId}`)?.name ?? null;
const benchProf = new URLSearchParams(location.search).has("bench") ? [] : null;
const view = createStreamView({
  prof: benchProf,
  scroller: $("#stream"),
  model: stream,
  agentName,
  onNeedMore: requestMore,
  onFollow: (on) => {
    // 回到底部恢復跟隨：載入更多時暫時保留的舊行修剪回上限
    if (on && stream.holding) view.update({ ...stream.trim(), appended: 0, updated: new Set() });
  },
});

function updateHasMore() {
  view.setHasMore([...stream.minPos().values()].some((p) => p > 1));
}

function toggleSession(id) {
  stream.setSessionHidden(id, !stream.hiddenSessions.has(id));
  view.filterChanged();
  render();
}

function toggleKind(k) {
  stream.setKindHidden(k, !stream.hiddenKinds.has(k));
  view.filterChanged();
  render();
}

$("#liveToggle").addEventListener("click", () => {
  const list = $("#liveList");
  list.hidden = !list.hidden;
  $("#liveToggle").setAttribute("aria-expanded", String(!list.hidden));
  renderLive();
});

// 載入更多：每個 session 各要一段（contracts.md §6 的 more），全部回來後依順序鍵合併放在最上面
const MORE_LIMIT = 200;
const MORE_TIMEOUT_MS = 3000;
let moreReq = null;

function requestMore() {
  if (moreReq) return;
  const wanted = [...stream.minPos()].filter(([, p]) => p > 1);
  if (!wanted.length) return;
  moreReq = { waiting: new Set(wanted.map(([s]) => s)), got: [], timer: setTimeout(finishMore, MORE_TIMEOUT_MS) };
  for (const [session, beforePos] of wanted) client.more({ session, beforePos, limit: MORE_LIMIT });
}

function onMore(m) {
  if (!moreReq || !moreReq.waiting.delete(m.session)) return;
  moreReq.got.push(m.events ?? []);
  if (moreReq.waiting.size === 0) finishMore();
}

function finishMore() {
  if (!moreReq) return;
  clearTimeout(moreReq.timer);
  const res = stream.prepend(moreReq.got);
  moreReq = null;
  view.prepended(res);
  updateHasMore();
}

function onStreamSnapshot(m) {
  stream.load({ staged: m.staged, stream: m.stream });
  moreReq = null;
  view.reset();
  updateHasMore();
}

function onStreamFrame(m) {
  const ops = stream.apply(m.events);
  view.update(ops);
  if (ops.dropped) updateHasMore();
  return ops;
}

// ---------- 效能量測（?mock=16&bench）：串流 ≥ 3000 行時，每幀套用 Event＋渲染＋排版的耗時 ----------
const bench = { on: false, samples: [], apply: [], js: [], events: 0, need: 120 };

function benchFrame(m) {
  const t0 = performance.now();
  const ops = onStreamFrame(m);
  const t1 = performance.now();
  view.flush();
  const t2 = performance.now();
  void $("#stream").scrollHeight; // 強制排版，把 style／layout 算進來
  const t3 = performance.now();
  if (stream.rows.length < 3000) return ops;
  bench.samples.push(t3 - t0);
  bench.apply.push(t1 - t0);
  bench.js.push(t2 - t0);
  bench.events += m.events.length;
  if (bench.samples.length === bench.need) {
    const q = (arr, p) => {
      const s = [...arr].sort((a, b) => a - b);
      return s[Math.min(s.length - 1, Math.floor(p * s.length))].toFixed(2);
    };
    const s = bench.samples;
    const text = `bench rows=${stream.rows.length} frames=${s.length} events/frame=${(bench.events / s.length).toFixed(1)} total p50=${q(s, 0.5)}ms p95=${q(s, 0.95)}ms max=${q(s, 1)}ms | apply p50=${q(bench.apply, 0.5)} js(apply+render) p50=${q(bench.js, 0.5)}`
      + `\nrender: pre p50=${q(benchProf.slice(-120).map((x) => x.pre), 0.5)} dom p50=${q(benchProf.slice(-120).map((x) => x.dom), 0.5)} painted avg=${(benchProf.slice(-120).reduce((a, x) => a + x.painted, 0) / 120).toFixed(1)} nodes=${benchProf.at(-1).nodes}`;
    const out = $("#benchOut");
    out.textContent = text;
    out.hidden = false;
    document.title = text;
    console.log(text);
  }
  return ops;
}

// ---------- 互動 ----------
function selectTab(id) {
  if (ui.tab !== "all") client.unsubscribe(`session:${ui.tab}`);
  ui.tab = id;
  if (id !== "all") client.subscribe(`session:${id}`);
  render();
}

for (const t of document.querySelectorAll(".tab")) {
  t.addEventListener("click", () => {
    for (const x of document.querySelectorAll(".tab")) x.classList.toggle("on", x === t);
    $("#tree").hidden = t.dataset.t !== "tree";
    $("#sess").hidden = t.dataset.t !== "sess";
  });
}

// ---------- 連線 ----------
const params = new URLSearchParams(location.search);
const mockN = params.has("mock") ? Math.max(1, Math.min(32, Number(params.get("mock")) || 12)) : 0;
bench.on = mockN > 0 && params.has("bench");
bench.isolate = bench.on && params.has("isolate");
const mock = mockN ? installMock(mockN, { bench: bench.on }) : null;
if (mock) {
  $("#mockDrop").hidden = false;
  $("#mockDrop").addEventListener("click", () => mock.restart(3000));
}

const proto = location.protocol === "https:" ? "wss:" : "ws:";
const client = createClient({
  url: `${proto}//${location.host}/ws`,
  WebSocket: mock ? mock.WebSocket : window.WebSocket,
  setTimeout: (fn, ms) => window.setTimeout(fn, ms),
  clearTimeout: (t) => window.clearTimeout(t),
  utcOffsetMin: -new Date().getTimezoneOffset(),
  handlers: {
    welcome: (m) => { ui.welcome = m; render(); },
    snapshot: (m) => {
      if (m.ch === "*") {
        applySnapshot(store, m);
        onStreamSnapshot(m);
      }
      render();
    },
    frame: (m) => {
      applyFrame(store, m);
      // session:<id> 頻道的 Event 與 `*` 重複，只有 `*` 進合流串流
      if (m.ch === "*") {
        const ops = bench.on ? benchFrame(m) : onStreamFrame(m);
        if (ops?.stagedChanged) render();
      }
      render();
    },
    more: onMore,
    onStatus: (s) => { if (ui.conn !== "incompatible") ui.conn = s; render(); },
    onIncompatible: () => { ui.conn = "incompatible"; render(); },
    onError: (e) => console.warn("ws message", e),
  },
});
client.subscribe("*");
client.connect();
render();
