import { createClient } from "./client.js";
import { createStore, applySnapshot, applyFrame } from "./store.js";
import { sessionColor, shortId } from "./color.js";
import { installMock } from "./mock-feed.js";
import { createStream } from "./stream.js";
import { createStreamView } from "./stream-view.js";
import { formatRow, esc } from "./format.js";
import { createChanges, diffRows, writeRows } from "./changes.js";
import { makeFloating } from "./floatwin.js";

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

const STATUS_TEXT = { run: "執行中", idle: "已中斷", wait: "等你", done: "待確認", fail: "失敗", danger: "危險", ended: "結束" };
const CONN_TEXT = { connecting: "連線中…", open: "已連線", closed: "已斷線，重新連線中…", incompatible: "collector 版本不相容，請重新整理或更新" };

const store = createStore();
const ui = { tab: "all", conn: "connecting", welcome: null };

// ---------- render ----------
// 每幀都會被叫到；內容沒變的面板不重建 DOM（避免整頁重新排版拖慢串流）
const lastSig = new Map();
function changed(name, sig) {
  if (lastSig.has(name) && bench.freeze.has(name)) return false; // 量測用：凍結指定面板
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
  // 進行中的 session，加上被隱藏的（即使已結束，也要能點回來顯示）
  const live = [...store.sessions.values()].filter((s) => s.status !== "ended" || stream.hiddenSessions.has(s.id));
  if (!changed("fSess", live.map((s) => `${s.id}:${s.status === "ended"}:${stream.hiddenSessions.has(s.id)}`).join("|"))) return;
  $("#fSess").replaceChildren(
    ...(live.length
      ? live.map((s) => {
          const off = stream.hiddenSessions.has(s.id);
          return h("button", { class: `fchip${off ? " off" : ""}${s.status === "ended" ? " ended" : ""}`, title: `${s.id}${s.status === "ended" ? "（已結束）" : ""}\n點一下${off ? "顯示" : "隱藏"}這個 session`, "aria-pressed": String(!off), onclick: () => toggleSession(s.id) },
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
  if (changed("liveN", String(stream.staged.size))) $("#liveN").textContent = String(stream.staged.size);
  const list = $("#liveList");
  if (!liveWin.isOpen || !changed("liveList", [...stream.staged.values()].slice(-100).map((c) => `${c.toolUseId}:${c.status}`).join("|"))) return;
  $("#liveWinTitle").textContent = `進行中的動作（${stream.staged.size}）`;
  const cards = [...stream.staged.values()].slice(-100);
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

// 每個 session 一列，只重建內容有變的那一列（Agents 的「目前動作」幾乎每幀都在變）
const agentRows = new Map(); // session id → { el, sig }

function renderAgents() {
  const bySession = new Map();
  for (const a of store.agents.values()) {
    if (!bySession.has(a.session)) bySession.set(a.session, []);
    bySession.get(a.session).push(a);
  }
  const live = liveSessions();
  if (bench.freeze.has("agents") && agentRows.size) return;
  const box = $("#agall");
  if (!live.length) {
    agentRows.clear();
    if (!changed("agentsEmpty", "1")) return;
    box.replaceChildren(h("div", { class: "none" }, "尚無 session"));
    return;
  }
  lastSig.delete("agentsEmpty");
  const rank = (a) => (a.agentId === "main" ? 0 : a.agentId === "bg" ? 1 : 2);
  const keep = new Set();
  let prev = null;
  for (const s of live) {
    keep.add(s.id);
    const list = (bySession.get(s.id) ?? []).sort((x, y) => rank(x) - rank(y) || (x.spawnedAt ?? 0) - (y.spawnedAt ?? 0));
    const sig = list.map((a) => `${a.id}:${a.status}:${a.name}:${a.current ?? ""}`).join("|");
    let row = agentRows.get(s.id);
    if (!row || row.sig !== sig) {
      const el = h("div", { class: "agrow" }, sessTag(s, "ss"), h("span", { class: "ags" }, ...(list.length ? list.map(agentChip) : [h("span", { class: "none" }, "—")])));
      if (row) row.el.replaceWith(el);
      row = { el, sig };
      agentRows.set(s.id, row);
    }
    // 順序跟 session 列表一致；已在正確位置就不動 DOM
    const want = prev ? prev.nextSibling : box.firstChild;
    if (want !== row.el) box.insertBefore(row.el, want);
    prev = row.el;
  }
  for (const [id, row] of agentRows) {
    if (!keep.has(id)) {
      row.el.remove();
      agentRows.delete(id);
    }
  }
  // 清掉「尚無 session」之類的殘留
  while (prev && prev.nextSibling) prev.nextSibling.remove();
}

// 串流以外的面板（KPI、tab、篩選、Sessions、Agents、即時計數）不需要每幀更新：
// 資料進來時最多每 PANEL_MS 畫一次；使用者操作與連線狀態改變（urgent）下一幀就畫。
// （試過把面板分到連續三幀畫，排版的固定成本變成付三次，p50／p95 反而變差，所以一次畫完。）
const PANEL_MS = 250;
let rafPending = false;
let throttleTimer = 0;
let lastPanelRender = 0;
let renderOnce = false;
function render(urgent = false) {
  if (bench.isolate && renderOnce) return; // 量測用：只畫第一次，之後其他面板不動
  if (rafPending) return;
  const wait = urgent ? 0 : lastPanelRender + PANEL_MS - performance.now();
  if (wait > 0) {
    if (!throttleTimer) throttleTimer = setTimeout(() => { throttleTimer = 0; render(true); }, wait);
    return;
  }
  clearTimeout(throttleTimer);
  throttleTimer = 0;
  rafPending = true;
  requestAnimationFrame(() => {
    rafPending = false;
    lastPanelRender = performance.now();
    renderOnce = stream.rows.length >= 3000;
    renderConn();
    renderKpis();
    renderTabs();
    renderSessFilters();
    renderKindFilters();
    renderSessions();
    renderAgents();
    renderLive();
    renderChanges();
    renderDiff();
  });
}

// ---------- 改動紀錄（中欄）與修改情形（右欄） ----------
const changes = createChanges();

function renderChanges() {
  if (!changed("chg", changes.selectedKey + "|" + changes.list.map((c) => c.key).join(","))) return;
  const box = $("#chg");
  if (!changes.list.length) {
    box.replaceChildren(h("div", { class: "none" }, "尚無改動"));
    return;
  }
  box.replaceChildren(
    ...(changes.following ? [] : [h("button", { class: "hint", onclick: () => selectChange(changes.list[0].key) }, "已停在選中的那筆；點這裡回到最新並恢復自動切換")]),
    ...changes.list.map((c) => {
      const n = c.kind === "edit" ? h("span", { class: "n" }, h("span", { class: "a" }, `+${c.added}`), " ", h("span", { class: "d" }, `−${c.removed}`)) : h("span", { class: "n" }, c.isNew ? "新檔案" : "覆寫");
      return h("button", { class: c.key === changes.selectedKey ? "sel" : "", title: `${c.path}\n${c.session}`, "aria-pressed": String(c.key === changes.selectedKey), onclick: () => selectChange(c.key) },
        h("span", { class: "ss", style: `color:${sessionColor(c.session)}` }, shortId(c.session)),
        h("span", { class: `k ${c.kind === "edit" ? "e" : "w"}` }, c.kind === "edit" ? "改" : "寫"),
        h("span", { class: "pth" }, c.path),
        n,
        h("span", { class: "ag" }, c.agent === "main" ? "main" : c.agent === "bg" ? "背景" : agentName(c.session, c.agent) ?? shortId(c.agent)));
    }),
  );
}

// diff 以資料存，只在選中時才轉成 HTML
function renderDiff() {
  const c = changes.selected();
  if (!changed("diff", c?.key ?? "")) return;
  $("#gfb").hidden = !c;
  $("#gfbNone").hidden = !!c;
  if (!c) return;
  const who = `${shortId(c.session)} · ${c.agent === "main" ? "main" : c.agent === "bg" ? "背景" : agentName(c.session, c.agent) ?? shortId(c.agent)}`;
  $("#fbKind").textContent = c.kind === "edit" ? `EDIT −${c.removed} +${c.added} · ${who}` : `WRITE${c.isNew ? "（新檔案）" : ""} · ${who}`;
  $("#fbPath").textContent = c.path;
  $("#fbPath").title = c.path;
  const rows = c.kind === "edit" ? diffRows(c.hunk, c.hunkSource) : writeRows(c.content, c.truncated);
  const no = (v) => (v == null ? "" : String(v));
  $("#fbBody").innerHTML = rows.map((r) => {
    if (r.type === "hunk") return `<div class="hunk">${esc(r.text)}</div>`;
    if (r.type === "note") return `<div class="note">${esc(r.text)}</div>`;
    if (r.type === "line") return `<div class="dl w"><span class="no">${r.new}</span><span class="tx">${esc(r.text)}</span></div>`;
    const mk = r.type === "add" ? "+" : r.type === "del" ? "−" : " ";
    return `<div class="dl ${r.type}"><span class="no">${no(r.old)}</span><span class="no">${no(r.new)}</span><span class="mk">${mk}</span><span class="tx">${esc(r.text)}</span></div>`;
  }).join("");
  $("#gfb").scrollTop = 0;
}

function selectChange(key) {
  changes.select(key);
  render(true);
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
  render(true);
}

function toggleKind(k) {
  stream.setKindHidden(k, !stream.hiddenKinds.has(k));
  view.filterChanged();
  render(true);
}

// 「即時：N 個動作進行中」點開成浮動視窗（不擋頁面、可拖動縮放）
const liveWin = makeFloating($("#liveWin"), {
  handle: $("#liveWinHandle"),
  key: "coding-lab.liveWin",
  initial: () => {
    const b = $("#liveToggle").getBoundingClientRect();
    return { x: b.left, y: Math.max(0, b.top - 340), w: 560, h: 320 };
  },
  onToggle: (open) => {
    $("#liveToggle").setAttribute("aria-expanded", String(open));
    if (open) {
      lastSig.delete("liveList"); // 剛打開：不管簽章，重畫一次
      renderLive();
    }
  },
});
$("#liveToggle").addEventListener("click", () => liveWin.toggle());
$("#liveWinClose").addEventListener("click", () => liveWin.close());

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
// &freeze=agents,kpi,…：第一次之後不再更新那些面板，用來找出是誰拖慢整頁
const bench = { on: false, samples: [], apply: [], js: [], events: 0, need: 120, freeze: new Set((new URLSearchParams(location.search).get("freeze") ?? "").split(",").filter(Boolean)) };

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
  render(true);
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
// &prefill：mock 先產生一段歷史（每個 session 約 220 個 pos），用來試「載入更多」與 3000 行上限
const mock = mockN ? installMock(mockN, { bench: bench.on, prefill: params.has("prefill") }) : null;
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
    welcome: (m) => { ui.welcome = m; render(true); },
    snapshot: (m) => {
      if (m.ch === "*") {
        applySnapshot(store, m);
        onStreamSnapshot(m);
        changes.load(m.changes);
      }
      render(true);
    },
    frame: (m) => {
      applyFrame(store, m);
      // session:<id> 頻道的 Event 與 `*` 重複，只有 `*` 進合流串流
      if (m.ch === "*") {
        if (bench.on) benchFrame(m);
        else onStreamFrame(m);
        changes.apply(m.events);
      }
      render();
    },
    more: onMore,
    onStatus: (s) => { if (ui.conn !== "incompatible") ui.conn = s; render(true); },
    onIncompatible: () => { ui.conn = "incompatible"; render(true); },
    onError: (e) => console.warn("ws message", e),
  },
});
client.subscribe("*");
client.connect();
render();
