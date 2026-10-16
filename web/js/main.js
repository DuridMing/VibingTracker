import { createClient } from "./client.js";
import { createStore, applySnapshot, applyFrame } from "./store.js";
import { sessionColor, shortId } from "./color.js";
import { installMock } from "./mock-feed.js";

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
  box.replaceChildren(
    h("button", { class: `stab${ui.tab === "all" ? " on" : ""}`, onclick: () => selectTab("all") }, "全部串流 ", h("small", {}, String(live.length))),
    ...live.map((s) =>
      h("button", { class: `stab ${s.status}${ui.tab === s.id ? " on" : ""}`, title: `${s.id}\n${s.prompt ?? ""}`, onclick: () => selectTab(s.id) },
        h("span", { class: "sw", style: `background:${sessionColor(s.id)}` }),
        h("span", { class: "dot" }),
        sessTag(s))),
  );
}

function renderSessFilters() {
  const live = liveSessions();
  $("#fSess").replaceChildren(
    ...(live.length
      ? live.map((s) => h("span", { class: "fchip", title: s.id }, h("i", { style: `background:${sessionColor(s.id)}` }), h("span", { class: "mono" }, shortId(s.id))))
      : [h("span", { class: "none" }, "無")]),
  );
}

function renderSessions() {
  const all = [...store.sessions.values()];
  $("#sessCount").textContent = all.length ? String(all.length) : "";
  $("#sess").replaceChildren(
    ...(all.length
      ? all.map((s) =>
          h("button", { class: ui.tab === s.id ? "sel" : "", onclick: () => selectTab(s.id) },
            sessTag(s),
            h("span", {}, h("span", { class: "pr" }, s.prompt ?? "—"), h("small", {}, `⎇ ${s.branch ?? "—"} · commits ${s.commits ?? 0}${s.dangers ? ` · ⚠ ${s.dangers}` : ""}`)),
            h("span", { class: `r ${s.status}` }, STATUS_TEXT[s.status] ?? s.status ?? "")))
      : [h("div", { class: "none" }, "尚無 session")]),
  );
}

function renderAgents() {
  const live = liveSessions();
  $("#agall").replaceChildren(
    ...(live.length
      ? live.map((s) => h("div", { class: "agrow" }, sessTag(s, "ss"), h("span", { class: "none" }, `${s.agents ?? 1} 個 agent（#641）`)))
      : [h("div", { class: "none" }, "尚無 session")]),
  );
}

let rafPending = false;
function render() {
  if (rafPending) return;
  rafPending = true;
  requestAnimationFrame(() => {
    rafPending = false;
    renderConn();
    renderKpis();
    renderTabs();
    renderSessFilters();
    renderSessions();
    renderAgents();
  });
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
const mock = mockN ? installMock(mockN) : null;
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
  handlers: {
    welcome: (m) => { ui.welcome = m; render(); },
    snapshot: (m) => { if (m.ch === "*") applySnapshot(store, m); render(); },
    frame: (m) => { applyFrame(store, m); render(); },
    onStatus: (s) => { if (ui.conn !== "incompatible") ui.conn = s; render(); },
    onIncompatible: () => { ui.conn = "incompatible"; render(); },
    onError: (e) => console.warn("ws message", e),
  },
});
client.subscribe("*");
client.connect();
render();
