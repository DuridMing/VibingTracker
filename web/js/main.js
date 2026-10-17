import { createClient } from "./client.js";
import { createStore, applySnapshot, applyFrame } from "./store.js";
import { sessionColor, shortId } from "./color.js";
import { installMock } from "./mock-feed.js";
import { createStream } from "./stream.js";
import { createStreamView } from "./stream-view.js";
import { formatRow, esc } from "./format.js";
import { createChanges, diffRows, writeRows } from "./changes.js";
import { makeFloating } from "./floatwin.js";
import { fmtKpi, modelBreakdown } from "./kpi.js";
import { sortAttention, sessionDangers, KIND_TEXT } from "./attention.js";
import { agentView, agentLabel, agentWho } from "./agents.js";
import { createSessionPane, nowOf, switchTab, sessionTitle, needsReplay } from "./session-pane.js";
import { parseMonitor, channelsFor, monitorUrl } from "./monitor.js";
import { loadSettings, saveSettings, sinksFor, summarize, createNotifier, createLeader } from "./notify.js";
import { createFileTree, filesUrl, sinceChanges } from "./tree.js";
import { mergeSessions, projectRoots, pickProject } from "./sessions.js";
import { degradedNotice } from "./degraded.js";

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
// #monitor=<id>：監視器視窗（#650），整頁只看一個 session、自己連 collector
const monitorId = parseMonitor(location.hash);
const ui = { tab: monitorId ?? "all", conn: "connecting", welcome: null };

// ---------- render ----------
// 每幀都會被叫到；內容沒變的面板不重建 DOM（避免整頁重新排版拖慢串流）
const lastSig = new Map();
function changed(name, sig) {
  if (lastSig.has(name) && bench.freeze.has(name)) return false; // 量測用：凍結指定面板
  if (lastSig.get(name) === sig) return false;
  lastSig.set(name, sig);
  return true;
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
  const many = Object.keys(store.kpi.models ?? {}).length > 0;
  for (const el of document.querySelectorAll("#kpis .km")) el.classList.toggle("has", many);
  if (kpop.key) fillKpop(kpop.key);
}

// 費用與思考 tokens 依模型分列（§6 kpi.models）：滑過或聚焦時顯示
const kpop = { key: null, anchor: null };
function fillKpop(key) {
  const el = $("#kpop");
  const rows = modelBreakdown(store.kpi.models, key);
  el.replaceChildren(
    h("div", { class: "kpop-h" }, key === "costUsd" ? "費用（今天）依模型" : "思考 tokens（今天）依模型"),
    ...(rows.length === 0
      ? [h("div", { class: "none" }, "collector 尚未提供依模型分列")]
      : rows.map((r) => h("div", { class: "kpop-r" },
          h("span", { class: "m", title: r.title }, r.model),
          h("i", { class: "bar" }, h("i", { style: `width:${(r.share * 100).toFixed(1)}%` })),
          h("b", {}, fmtKpi(key, r.value)),
          h("small", {}, `${Math.round(r.share * 100)}%`)))),
  );
}
function openKpop(anchor) {
  const el = $("#kpop");
  kpop.key = anchor.dataset.m;
  kpop.anchor = anchor;
  fillKpop(kpop.key);
  el.hidden = false;
  const a = anchor.getBoundingClientRect();
  const w = el.offsetWidth;
  el.style.left = `${Math.max(8, Math.min(a.right - w, innerWidth - w - 8))}px`;
  el.style.top = `${a.bottom + 6}px`;
}
function closeKpop(anchor) {
  if (kpop.anchor !== anchor) return;
  kpop.key = kpop.anchor = null;
  $("#kpop").hidden = true;
}
for (const el of document.querySelectorAll("#kpis .km")) {
  el.addEventListener("mouseenter", () => openKpop(el));
  el.addEventListener("focus", () => openKpop(el));
  el.addEventListener("mouseleave", () => closeKpop(el));
  el.addEventListener("blur", () => closeKpop(el));
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
  if (!changed("tabs", ui.tab + "|" + live.map((s) => `${s.id}:${s.status}:${s.lastPrompt ?? ""}:${s.doing ?? ""}`).join("|"))) return;
  box.replaceChildren(
    h("button", { class: `stab${ui.tab === "all" ? " on" : ""}`, onclick: () => selectTab("all") }, "全部串流 ", h("small", {}, String(live.length))),
    ...live.map((s) =>
      h("button", { class: `stab ${s.status}${ui.tab === s.id ? " on" : ""}`, title: sessionTitle(s), onclick: () => selectTab(s.id) },
        h("span", { class: "sw", style: `background:${sessionColor(s.id)}` }),
        h("span", { class: "dot" }),
        sessTag(s),
        h("span", { class: "pop", role: "button", tabindex: "0", title: "開成監視器視窗（可拖到第二螢幕，關掉主視窗也能繼續看）", onclick: (e) => { e.stopPropagation(); openMonitor(s.id); } }, "⧉"))),
  );
}

function openMonitor(id) {
  window.open(monitorUrl(location, id), `coding-lab-monitor-${id}`, "popup,width=980,height=760");
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

// Sessions tab：WS 的列加上 /api/sessions 補的已結束 session（mergeSessions，新到舊）
const SESS_SHOWN = 200;
function renderSessions() {
  const all = allSessions();
  if (!changed("sess", ui.tab + "|" + all.map((s) => `${s.id}:${s.status}:${s.firstPrompt}:${s.branch}:${s.commits}:${s.dangers}:${s.model}:${s.doing ?? ""}`).join("|"))) return;
  $("#sessCount").textContent = all.length ? String(all.length) : "";
  $("#sess").replaceChildren(
    ...(all.length
      ? all.slice(0, SESS_SHOWN).map((s) =>
          h("button", { class: `${ui.tab === s.id ? "sel" : ""}${s.status === "ended" ? " ended" : ""}`, title: sessionTitle(s), onclick: () => selectTab(s.id) },
            sessTag(s),
            h("span", {}, h("span", { class: "pr" }, s.firstPrompt ?? "—"),
              s.doing && s.status !== "ended" ? h("small", { class: "doing" }, `▸ ${s.doing}`) : null,
              h("small", {}, `⎇ ${s.branch ?? "—"} · commits ${s.commits ?? 0}${s.dangers ? ` · ⚠ ${s.dangers}` : ""}`, s.model ? h("span", { class: "mdl", title: s.modelId ?? s.model }, s.model) : null)),
            h("span", { class: `r ${s.status}` }, STATUS_TEXT[s.status] ?? s.status ?? "")))
      : [h("div", { class: "none" }, "尚無 session")]),
    ...(all.length > SESS_SHOWN ? [h("div", { class: "more" }, `只列最近 ${SESS_SHOWN} 個（共 ${all.length}）`)] : []),
  );
}

const AGENT_STATUS_TEXT = { run: "執行中", idle: "閒置", done: "完成", fail: "失敗", danger: "注意" };

// 每個 session 一列：main、背景、執行中與出事的 subagent 各一個 chip；已完成的收成「✓ n」（contracts.md §6）。
function agentChip(a) {
  const isBg = a.agentId === "bg";
  const title = [a.label, a.agentType, a.model && (a.modelId ?? a.model), a.task, AGENT_STATUS_TEXT[a.status], a.current].filter(Boolean).join("\n");
  return h("span", { class: `agc ${a.status}${isBg ? " bg" : ""}`, title },
    h("i"), h("b", {}, a.label),
    a.model ? h("small", { class: "mdl", title: a.modelId ?? a.model }, a.model) : null,
    !isBg && a.current ? h("span", { class: "cur" }, a.current) : null);
}

function doneChip(done) {
  return h("span", { class: "agc fin", title: `已完成的 subagent（${done.length}）\n` + done.map((a) => `· ${a.label}`).join("\n") }, h("i"), `✓ ${done.length}`);
}

// 剛完成的 subagent 在標籤列停 LINGER_MS：期間每秒重畫一次，時間到就收進「✓ n」
let lingerTimer = 0;
function lingerRecheck() {
  if (lingerTimer) return;
  lingerTimer = setTimeout(() => { lingerTimer = 0; render(true); }, 1000);
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
  const keep = new Set();
  let prev = null;
  for (const s of live) {
    keep.add(s.id);
    const v = agentView(bySession.get(s.id) ?? [], s.id, { sessionModel: s.model, now: Date.now() });
    if (v.recent.length) lingerRecheck();
    const chips = [v.main, v.bg, ...v.active, ...v.recent].filter(Boolean);
    const sig = chips.map((a) => `${a.id}:${a.status}:${a.label}:${a.model ?? ""}:${a.current ?? ""}`).join("|") + `|done:${v.done.map((a) => a.id).join(",")}|${s.doing ?? ""}`;
    let row = agentRows.get(s.id);
    if (!row || row.sig !== sig) {
      const parts = chips.map(agentChip);
      if (v.done.length) parts.push(doneChip(v.done));
      const el = h("div", { class: "agrow", title: sessionTitle(s) }, sessTag(s, "ss"),
        h("span", { class: "agb" },
          s.doing ? h("span", { class: "doing" }, `▸ ${s.doing}`) : null,
          h("span", { class: "ags" }, ...(parts.length ? parts : [h("span", { class: "none" }, "—")]))));
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
// transcript 降級（#652）：collector 的 sessions 列說哪些 session 讀不到 transcript，串流上方標出來
let lastDegraded = "";
function renderDegraded() {
  const el = document.getElementById("degradedNote");
  if (!el) return;
  const n = degradedNotice(store.sessions.values());
  const text = n ? n.text : "";
  if (text === lastDegraded) return;
  lastDegraded = text;
  el.textContent = text;
  el.hidden = !n;
}

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
    renderDegraded();
    renderKpis();
    renderTabs();
    renderSessFilters();
    renderKindFilters();
    renderSessions();
    renderTree();
    renderAgents();
    renderLive();
    renderChanges();
    renderDiff();
    renderAttention();
    renderNotifySwitches();
    renderPane();
  });
}

// ---------- 注意佇列（#646） ----------
// 排序：種類 > session 分數 > raisedAt（attention.js）。「已確認」送 ack，等 collector 送 deleted 才真的移除；送出後到刪除前先變淡。
const ackPending = new Set();
const scoreOf = (id) => store.sessions.get(id)?.score;

function ago(ms) {
  const m = Math.floor(ms / 60_000);
  if (m < 1) return "剛剛";
  return m < 60 ? `${m} 分` : `${Math.floor(m / 60)} 時`;
}

function renderAttention() {
  for (const id of ackPending) if (!store.attention.has(id)) ackPending.delete(id);
  const rows = sortAttention([...store.attention.values()], scoreOf);
  const now = Date.now();
  const canAck = ui.welcome?.scope !== "view";
  const sig = `${Math.floor(now / 30_000)}|${canAck}|${ui.tab}|` + rows.map((r) => `${r.id}:${scoreOf(r.session) ?? 0}:${ackPending.has(r.id)}:${r.reason}:${r.clearedAt ?? ""}`).join(",");
  if (!changed("att", sig)) return;
  const box = $("#att");
  if (!rows.length) {
    box.replaceChildren(h("div", { class: "none" }, "目前沒有需要處理的 session"));
    return;
  }
  box.replaceChildren(...rows.map((r) => {
    const s = store.sessions.get(r.session);
    const pend = ackPending.has(r.id);
    const score = scoreOf(r.session);
    const ack = { class: "btn sm", title: "已確認：從佇列移除（所有視窗一致）", onclick: (e) => { e.stopPropagation(); ackRow(r); } };
    if (pend) ack.disabled = "";
    return h("div", {
      class: `arow ${r.kind}${r.clearedAt != null ? " cleared" : ""}${pend ? " pend" : ""}${ui.tab === r.session ? " on" : ""}`, role: "button", tabindex: "0",
      title: `${r.session}${s?.lastPrompt ? `\n${s.lastPrompt}` : ""}${score ? `\n分數 ${score}` : ""}${r.clearedAt != null ? "\n工具已結束，等你確認" : ""}`,
      onclick: () => selectTab(r.session),
      onkeydown: (e) => { if (e.key === "Enter" && e.target === e.currentTarget) selectTab(r.session); },
    },
    h("span", { class: "ak" }, KIND_TEXT[r.kind] ?? r.kind),
    h("span", { class: "ss", style: `color:${sessionColor(r.session)}` }, shortId(r.session)),
    h("span", { class: "ar" }, r.reason ?? ""),
    h("span", { class: "at" }, r.clearedAt != null ? "已結束" : ago(now - (r.raisedAt ?? now))),
    h("button", { class: "btn sm", onclick: (e) => { e.stopPropagation(); selectTab(r.session); } }, "查看"),
    canAck ? h("button", ack, pend ? "確認中…" : "已確認") : null);
  }));
}

function ackRow(r) {
  ackPending.add(r.id);
  client.ack({ session: r.session, kind: r.kind, ref: r.ref });
  render(true);
}

// ---------- 通知（#647） ----------
// toast 每個主視窗各自顯示；聲音與瀏覽器通知只由 leader 發；#monitor 視窗都不發。不補響、1 秒合併在 notify.js。
const isMonitor = monitorId != null;
const storage = (() => { try { return window.localStorage; } catch { return null; } })();
const nset = loadSettings(storage);
const notifyUi = { reason: "" }; // 開關旁的說明（不能用的原因、點一下啟用聲音）

function canBrowserNotify() {
  if (!("Notification" in window)) return "這個瀏覽器不支援通知";
  if (!window.isSecureContext) return "非 HTTPS 的遠端連線不能用瀏覽器通知";
  if (Notification.permission === "denied") return "瀏覽器已封鎖這個網站的通知";
  return "";
}

let audio = null;
function audioCtx() {
  if (audio) return audio;
  const AC = window.AudioContext ?? window.webkitAudioContext;
  if (!AC) return null;
  try { audio = new AC(); } catch { return null; }
  return audio;
}
// 完成兩聲上揚、等你一聲中、危險（與失敗）一聲低
const TONES = { done: [[660, 0], [880, 0.16]], wait: [[620, 0]], danger: [[220, 0]], fail: [[220, 0]], conflict: [[620, 0]] };
function playTone(kind) {
  const ctx = audioCtx();
  if (!ctx || ctx.state !== "running") return;
  for (const [freq, at] of TONES[kind] ?? TONES.wait) {
    const t = ctx.currentTime + at;
    const o = ctx.createOscillator();
    const g = ctx.createGain();
    o.type = "sine";
    o.frequency.value = freq;
    g.gain.setValueAtTime(0.0001, t);
    g.gain.exponentialRampToValueAtTime(0.2, t + 0.01);
    g.gain.exponentialRampToValueAtTime(0.0001, t + 0.18);
    o.connect(g).connect(ctx.destination);
    o.start(t);
    o.stop(t + 0.2);
  }
}
// 自動播放政策：重新整理後要使用者先互動，AudioContext 才會 running
function unlockAudio() {
  if (!nset.sound) return;
  const ctx = audioCtx();
  if (ctx && ctx.state !== "running") ctx.resume().then(() => render(true), () => {});
}
addEventListener("pointerdown", unlockAudio, { capture: true });
addEventListener("keydown", unlockAudio, { capture: true });

function showToast(sum, items) {
  const box = $("#toasts");
  const el = h("button", { class: `toast ${sum.kind}`, onclick: () => { selectTab(items[0].session); el.remove(); } },
    h("b", {}, sum.title), sum.body ? h("span", {}, sum.body) : null);
  box.prepend(el);
  while (box.children.length > 4) box.lastChild.remove();
  setTimeout(() => el.remove(), 6000);
}

function showBrowserNote(sum, items) {
  if (canBrowserNotify() || Notification.permission !== "granted") return;
  try {
    const n = new Notification(sum.title, { body: sum.body, tag: "coding-lab", silent: true });
    n.onclick = () => { window.focus(); selectTab(items[0].session); n.close(); };
  } catch {
    // 有些瀏覽器只允許從 service worker 發；這裡不處理
  }
}

const leader = createLeader({
  id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
  channel: !isMonitor && "BroadcastChannel" in window ? new BroadcastChannel(`coding-lab.notify${location.search.includes("mock") ? ".mock" : ""}`) : null,
  now: () => Date.now(),
  setInterval: (fn, ms) => window.setInterval(fn, ms),
  clearInterval: (t) => window.clearInterval(t),
});
addEventListener("pagehide", () => leader.stop());

const notifier = createNotifier({
  now: () => Date.now(),
  setTimeout: (fn, ms) => window.setTimeout(fn, ms),
  clearTimeout: (t) => window.clearTimeout(t),
  emit: ({ items }) => {
    const sum = summarize(items, shortId);
    for (const sink of sinksFor(nset, { isLeader: leader.isLeader(), isMonitor })) {
      if (sink === "toast") showToast(sum, items);
      else if (sink === "sound") playTone(sum.kind);
      else if (sink === "browser") showBrowserNote(sum, items);
    }
  },
});

function renderNotifySwitches() {
  const why = canBrowserNotify();
  const soundLocked = nset.sound && audio?.state !== "running";
  const hint = nset.browser && why ? why : soundLocked ? "點一下啟用聲音" : notifyUi.reason;
  if (!changed("nsw", `${nset.toast}${nset.sound}${nset.browser}|${why}|${hint}`)) return;
  $("#nToast").checked = nset.toast;
  $("#nSound").checked = nset.sound;
  $("#nBrowser").checked = nset.browser && !why;
  $("#nBrowser").disabled = !!why && why !== "瀏覽器已封鎖這個網站的通知";
  $("#nBrowserL").title = why;
  $("#nHint").hidden = !hint;
  $("#nHint").textContent = hint;
}

function setNotify(key, on) {
  nset[key] = on;
  notifyUi.reason = "";
  saveSettings(storage, nset);
  render(true);
}
$("#nToast").addEventListener("change", (e) => setNotify("toast", e.target.checked));
$("#nSound").addEventListener("change", (e) => {
  setNotify("sound", e.target.checked);
  unlockAudio(); // 勾選本身就是使用者互動
});
$("#nBrowser").addEventListener("change", async (e) => {
  const on = e.target.checked;
  if (on && !canBrowserNotify() && Notification.permission === "default") {
    const p = await Notification.requestPermission().catch(() => "denied");
    if (p !== "granted") {
      setNotify("browser", false);
      notifyUi.reason = "沒有取得通知權限";
      render(true);
      return;
    }
  }
  setNotify("browser", on && !canBrowserNotify());
});
$("#nHint").addEventListener("click", unlockAudio);
$("#spPop").addEventListener("click", () => openMonitor(pane.session));
setInterval(() => render(), 30_000); // 佇列的「幾分前」

// ---------- 左欄：專案檔案（#642）與改動基準（#644） ----------
// 樹與 git 狀態來自 GET /api/files（tree.js 的 filesUrl）；方塊來自 `*` 頻道的 file.* Event（tree.js 累積）。
// 「本次開啟以來」不問 collector：用這個頁面收到的 Event 累積（sinceChanges）。
// 專案：session 的 cwd 歸組（sessions.js），一次看一個；每個專案一棵樹，Event 先收在 fileEvents，開新樹時重放。
const ROW_H = 22;
const FILE_EVENTS_MAX = 50_000;
const BASE_TEXT = { main: "相對 main", since: "本次開啟以來", session: "單一 session", commit: "相對 commit" };
const pref = {
  get: (k) => { try { return storage?.getItem(`coding-lab.tree.${k}`) ?? null; } catch { return null; } },
  set: (k, v) => { try { storage?.setItem(`coding-lab.tree.${k}`, v); } catch { /* 無痕模式等 */ } },
};
const left = {
  tab: "tree",
  fetched: [], // GET /api/sessions 的列
  project: pref.get("project"),
  base: BASE_TEXT[pref.get("base")] ? pref.get("base") : "main",
  baseSess: null,
  hash: "",
  hashCwd: null, // 從單 session 的 git 列點 commit 時，用那個 session 的 worktree
  trees: new Map(), // 專案 cwd → { tree, loading, error, info }
  fileEvents: [],
  req: 0,
  dirty: true,
  rows: [],
  els: new Map(), // path → { el, sig }
  flash: new Map(), // path → 閃爍的 class（下次畫時套上）
  lit: new Set(), // 被點亮的父目錄
  refetch: 0,
};
const fetchedById = () => new Map(left.fetched.map((s) => [s.id, s]));
let fetchedIndex = new Map();
const cwdOf = (id) => store.sessions.get(id)?.cwd ?? fetchedIndex.get(id)?.cwd ?? null;
const apiFetch = (url) => (mock ? mock.fetch(url) : fetch(url, { credentials: "same-origin" }));
const normRoot = (p) => String(p ?? "").replace(/\\/g, "/").replace(/\/+$/, "");

function allSessions() {
  return mergeSessions([...store.sessions.values()], left.fetched);
}

function treeFor(cwd) {
  let ent = left.trees.get(cwd);
  if (!ent) {
    const tree = createFileTree();
    tree.cwdOf = cwdOf;
    tree.apply(left.fileEvents); // 樹還沒載入：先收著，setFiles 時才算
    ent = { tree, loading: false, error: null, info: "" };
    left.trees.set(cwd, ent);
  }
  return ent;
}

// 專案清單變了（新 session、Sessions 補進來的）：選一個，換了就重抓
function syncProjects() {
  const roots = projectRoots(allSessions());
  const pick = pickProject(roots, left.project);
  if (pick && pick !== left.project) {
    left.project = pick;
    left.dirty = true;
    loadFiles();
  } else if (pick && !left.trees.has(pick)) loadFiles();
}

const FLASH = { r: "flash", e: "flash-e", w: "flash-w", x: "flash-x" };
// `*` 的 Event（snapshot 的 stream／changes、frame 的 events）：只留 file.* 與失敗，餵給每一棵樹
function feedFiles(events) {
  if (isMonitor) return;
  const mine = (events ?? []).filter((e) => e.type?.startsWith("file.") || (e.type === "tool.updated" && e.payload?.status === "failed"));
  const gitOp = (events ?? []).some((e) => e.type === "git.op");
  if (!mine.length && !gitOp) return;
  left.fileEvents.push(...mine);
  if (left.fileEvents.length > FILE_EVENTS_MAX) left.fileEvents.splice(0, left.fileEvents.length - FILE_EVENTS_MAX);
  let changedFiles = gitOp;
  for (const [cwd, ent] of left.trees) {
    const hits = ent.tree.apply(mine);
    if (cwd !== left.project || !hits.length) continue;
    left.dirty = true;
    for (const hit of hits) {
      left.flash.set(hit.path, FLASH[hit.kind]);
      const parts = hit.path.split("/");
      for (let i = 1; i < parts.length; i++) left.lit.add(parts.slice(0, i).join("/"));
      if (hit.kind === "e" || hit.kind === "w") changedFiles = true;
    }
  }
  if (!changedFiles) return;
  if (left.base === "since") {
    const ent = left.trees.get(left.project);
    if (ent?.tree.root != null) setSince(ent);
  } else scheduleFiles(3000); // git 狀態跟著改動與 commit 變，等一下再抓（合併連續的改動）
}

function setSince(ent) {
  const ch = sinceChanges(ent.tree);
  ent.tree.setGit(ch);
  ent.info = `本次開啟以來 · ${ch.length} 個檔有改動`;
  left.dirty = true;
}

function scheduleFiles(ms) {
  clearTimeout(left.refetch);
  left.refetch = setTimeout(loadFiles, ms);
}

function currentBase() {
  if (left.base === "session") {
    const id = left.baseSess ?? (ui.tab !== "all" ? ui.tab : projectSessions()[0]?.id);
    return id ? { kind: "session", id } : null;
  }
  if (left.base === "commit") return { kind: "commit", hash: left.hash, cwd: left.hashCwd ?? undefined };
  return { kind: left.base };
}

function projectSessions() {
  const root = left.project;
  if (!root) return [];
  return allSessions().filter((s) => s.cwd && (s.cwd === root || s.cwd.startsWith(`${root}/`)));
}

async function loadFiles() {
  clearTimeout(left.refetch);
  const cwd = left.project;
  if (!cwd) return;
  const ent = treeFor(cwd);
  const base = currentBase();
  // 樹還沒載入時，commit 基準先用專案的 cwd（另一個 worktree 的根不同，樹要以專案為準）
  const url = base && filesUrl(cwd, ent.tree.root == null && base.kind === "commit" ? { ...base, cwd: undefined } : base);
  if (!url) {
    ent.error = base ? "commit hash 要 4–64 位十六進位" : "這個專案沒有 session";
    render(true);
    return;
  }
  const req = ++left.req;
  ent.loading = true;
  render(true);
  try {
    const res = await apiFetch(url);
    const body = await res.json().catch(() => ({}));
    if (req !== left.req) return;
    if (!res.ok) throw new Error(body.error ?? `HTTP ${res.status}`);
    // 專案的根以第一次為準；另一個 worktree 的 commit 基準只拿它的 changes（路徑一樣相對 repo 根）
    if (ent.tree.root == null || normRoot(body.root) === ent.tree.root) ent.tree.setFiles(body);
    const n = body.changes?.length ?? 0;
    const cf = (body.changes ?? []).filter((c) => c.conflict).length;
    const trunc = body.treeTruncated ? " · 檔案太多，樹只列前 20000 個" : "";
    if (base.kind === "since") setSince(ent);
    else if (base.kind === "session") {
      ent.tree.setGit(body.changes, (p) => ent.tree.pathOf(cwdOf(base.id), p));
      ent.info = `session ${shortId(base.id)} · ${n} 個檔有改動`;
    } else {
      ent.tree.setGit(body.changes);
      const wts = new Set((body.changes ?? []).flatMap((c) => c.by.map((b) => b.worktree)));
      ent.info = base.kind === "commit"
        ? `相對 ${String(body.baseRef ?? base.hash).slice(0, 7)} · ${n} 個檔有改動`
        : `相對 ${body.baseRef ?? "main"} · ${n} 個檔有改動${wts.size > 1 ? `（${wts.size} 個 worktree）` : ""}${cf ? ` · ⚠ ${cf} 個衝突候選` : ""}`;
    }
    ent.info += trunc;
    ent.error = null;
    left.dirty = true;
  } catch (e) {
    if (req !== left.req) return;
    ent.error = `檔案樹載入失敗：${e.message}`;
  } finally {
    if (req === left.req) {
      ent.loading = false;
      render(true);
    }
  }
}
setInterval(() => { if (left.tab === "tree" && !document.hidden && left.base !== "since") loadFiles(); }, 60_000);

async function loadSessions() {
  if (isMonitor) return;
  try {
    const res = await apiFetch("/api/sessions?limit=500");
    if (!res.ok) return;
    const body = await res.json();
    left.fetched = Array.isArray(body.sessions) ? body.sessions : [];
    fetchedIndex = fetchedById();
    syncProjects();
    render(true);
  } catch {
    // collector 太舊或斷線：只顯示 WS 的列
  }
}
setInterval(() => { if (!document.hidden) loadSessions(); }, 60_000);

function renderTreeBar() {
  const roots = projectRoots(allSessions());
  const ent = left.project ? left.trees.get(left.project) : null;
  const sess = left.base === "session" ? projectSessions() : [];
  const base = currentBase();
  const sig = JSON.stringify([roots.map((r) => [r.cwd, r.live]), left.project, left.base, base?.id, sess.map((s) => s.id), left.hash, ent?.info, ent?.error, ent?.loading]);
  if (!changed("tbar", sig)) return;
  const proj = $("#tProj");
  proj.hidden = roots.length < 2;
  proj.replaceChildren(...roots.map((r) => {
    const o = h("option", { value: r.cwd, title: r.cwd }, `${r.cwd.split("/").pop() || r.cwd}（${r.live}/${r.sessions.length}）`);
    if (r.cwd === left.project) o.selected = true;
    return o;
  }));
  $("#tBase").value = left.base;
  const bs = $("#tBaseSess");
  bs.hidden = left.base !== "session";
  bs.replaceChildren(...sess.map((s) => {
    const o = h("option", { value: s.id }, `${shortId(s.id)} ${s.firstPrompt ?? ""}`.slice(0, 40));
    if (s.id === base?.id) o.selected = true;
    return o;
  }));
  const hash = $("#tBaseHash");
  hash.hidden = left.base !== "commit";
  if (document.activeElement !== hash) hash.value = left.hash;
  hash.classList.toggle("bad", left.base === "commit" && !!left.hash && !/^[0-9a-f]{4,64}$/i.test(left.hash));
  const info = $("#tInfo");
  info.classList.toggle("err", !!ent?.error);
  info.textContent = ent?.error ?? (ent?.loading && !ent.info ? "載入中…" : left.base === "commit" && !left.hash ? "輸入 commit hash，或在單一 session 的 git 列點最後的 commit" : ent?.info ?? "");
  info.title = info.textContent;
}

function setBase(kind) {
  left.base = kind;
  pref.set("base", kind);
  if (kind !== "commit") left.hashCwd = null;
  const ent = left.trees.get(left.project);
  if (kind === "since" && ent?.tree.root != null) {
    setSince(ent);
    render(true);
  } else if (kind !== "commit" || /^[0-9a-f]{4,64}$/i.test(left.hash)) loadFiles();
  else render(true);
}
$("#tProj").addEventListener("change", (e) => {
  left.project = e.target.value;
  pref.set("project", left.project);
  left.dirty = true;
  left.els.clear();
  $("#tree").replaceChildren();
  loadFiles();
});
$("#tBase").addEventListener("change", (e) => setBase(e.target.value));
$("#tBaseSess").addEventListener("change", (e) => { left.baseSess = e.target.value; loadFiles(); });
$("#tBaseHash").addEventListener("change", (e) => {
  left.hash = e.target.value.trim();
  left.hashCwd = null;
  setBase("commit");
});

// 單 session 的 git 列：點最後的 commit，左欄改成「相對這個 commit」
$("#spLast").addEventListener("click", () => {
  const c = pane.git.last;
  if (!c?.hash) return;
  left.hash = String(c.hash);
  left.hashCwd = cwdOf(pane.session);
  showLeftTab("tree");
  setBase("commit");
});

function showLeftTab(name) {
  left.tab = name;
  for (const x of document.querySelectorAll(".tab")) x.classList.toggle("on", x.dataset.t === name);
  $("#tree").hidden = name !== "tree";
  $("#tbar").hidden = name !== "tree";
  $("#treeLeg").hidden = name !== "tree";
  $("#sess").hidden = name !== "sess";
  if (name === "sess") loadSessions();
  else left.dirty = true;
  render(true);
}

function treePlaceholder(text) {
  left.els.clear();
  if (changed("treeNone", text)) $("#tree").replaceChildren(h("div", { class: "none" }, text));
}

// 只畫看得到的列（固定列高）；列的內容沒變就不重建，閃爍與點亮在下一次畫時套上
function renderTree() {
  if (left.tab !== "tree" || isMonitor) return;
  renderTreeBar();
  const ent = left.project ? left.trees.get(left.project) : null;
  if (!ent || ent.tree.root == null) {
    treePlaceholder(ent?.error ?? (left.project ? "載入檔案樹…" : "尚無 session"));
    return;
  }
  lastSig.delete("treeNone");
  const t = ent.tree;
  const box = $("#tree");
  if (left.dirty) {
    left.rows = t.visible();
    left.dirty = false;
  }
  const rows = left.rows;
  let inner = box.firstElementChild;
  if (!inner || inner.className !== "tin") {
    inner = h("div", { class: "tin" });
    box.replaceChildren(inner);
    left.els.clear();
  }
  if (!rows.length) {
    inner.style.height = "";
    inner.replaceChildren(h("div", { class: "none" }, "這個專案沒有檔案（不是 git repo？）"));
    left.els.clear();
    return;
  }
  inner.style.height = `${rows.length * ROW_H}px`;
  const top = box.scrollTop;
  const height = box.clientHeight || 600;
  const from = Math.max(0, Math.floor(top / ROW_H) - 10);
  const to = Math.min(rows.length, Math.ceil((top + height) / ROW_H) + 10);
  const running = new Set([...store.sessions.values()].filter((s) => s.status === "run").map((s) => s.id));
  const keep = new Set();
  for (let i = from; i < to; i++) {
    const r = rows[i];
    keep.add(r.path);
    const c = r.dir ? null : t.counts(r.path);
    const dots = r.dir ? [] : t.touching(r.path, running);
    const sig = `${i}|${r.pre}|${r.open}|${r.git}|${r.conflict}|${c ? `${c.r},${c.e},${c.w},${c.x},${c.isNew}` : ""}|${dots.join(",")}`;
    let row = left.els.get(r.path);
    if (!row || row.sig !== sig) {
      const el = treeRow(r, c, dots, t, i);
      if (row) row.el.replaceWith(el);
      else inner.append(el);
      row = { el, sig };
      left.els.set(r.path, row);
    }
    const fl = left.flash.get(r.path);
    if (fl || left.lit.has(r.path)) {
      const el = row.el;
      el.classList.remove("flash", "flash-e", "flash-w", "flash-x", "lit");
      void el.offsetWidth; // 重新觸發動畫
      if (fl) el.classList.add(fl);
      if (left.lit.has(r.path)) el.classList.add("lit");
    }
  }
  for (const [p, row] of left.els) {
    if (!keep.has(p)) {
      row.el.remove();
      left.els.delete(p);
    }
  }
  left.flash.clear();
  left.lit.clear();
}

function treeRow(r, c, dots, t, i) {
  const cls = `node ${r.dir ? "dir" : "file"}${r.git ? ` g-${r.git}` : ""}${c?.isNew ? " isnew" : ""}${r.conflict && !r.dir ? " cflt" : ""}`;
  const lbl = h("span", { class: "lbl" }, h("span", { class: "pre" }, r.pre), h("span", { class: "nm" }, r.dir ? `${r.name}/` : r.name), r.conflict ? h("span", { class: "cf" }, "⚠") : null);
  const mk = h("span", { class: "mk" });
  if (c) {
    const cap = (n) => Math.min(n, 6);
    for (const [k, n] of [["r", c.r], ["e", c.e], ["w", c.w], ["x", c.x]]) for (let j = 0; j < cap(n); j++) mk.append(h("i", { class: k }));
    const total = c.r + c.e + c.w + c.x;
    if ([c.r, c.e, c.w, c.x].some((n) => n > 6)) mk.append(h("small", {}, String(total)));
  }
  if (dots.length) mk.append(h("span", { class: "sb" }, ...dots.map((s) => h("i", { style: `background:${sessionColor(s)}`, title: s }))));
  const attrs = { class: cls, style: `top:${i * ROW_H}px`, title: treeTitle(r, c, dots, t) };
  if (r.dir) {
    attrs["aria-expanded"] = String(r.open);
    attrs.onclick = () => {
      t.toggle(r.path);
      left.dirty = true;
      render(true);
    };
    return h("button", attrs, lbl, mk);
  }
  return h("div", attrs, lbl, mk);
}

const GIT_TEXT = { M: "修改", A: "新增", "?": "未追蹤", D: "刪除", R: "改名", C: "複製", T: "類型改變" };
function treeTitle(r, c, dots, t) {
  const lines = [r.path];
  if (c && c.r + c.e + c.w + c.x) lines.push(`讀 ${c.r} · 改 ${c.e} · 新建 ${c.w} · 失敗 ${c.x}`);
  const m = r.dir ? null : t.mark(r.path);
  for (const b of m?.by ?? []) {
    const who = b.session ? shortId(b.session) : b.branch ?? b.worktree ?? "";
    lines.push(`${who}：${GIT_TEXT[b.status] ?? b.status}${b.from ? `（從 ${b.from}）` : ""}`);
  }
  if (r.conflict) lines.push(r.dir ? "⚠ 底下有衝突候選" : "⚠ 衝突候選：兩個 worktree 都改了這個檔，內容不同");
  if (dots.length) lines.push(`正在碰：${dots.map(shortId).join("、")}`);
  return lines.join("\n");
}

let treeScrollRaf = 0;
$("#tree").addEventListener("scroll", () => {
  if (treeScrollRaf) return;
  treeScrollRaf = requestAnimationFrame(() => {
    treeScrollRaf = 0;
    renderTree();
  });
}, { passive: true });

// ---------- 單 session 畫面（#649）：tab 切到某個 session 時取代全部串流那一塊 ----------
// 資料只來自 `session:<id>` 頻道（session-pane.js）；NOW 與 lanes 用 collector 的 agents 列，跟 Agents 面板同一份。
const pane = createSessionPane();
let paneMoreReq = null;
const NOW_TEXT = { run: "執行中", wait: "等你", done: "完成", fail: "失敗", danger: "危險", idle: "閒置", ended: "結束" };
const LANE_TEXT = { run: "執行中", idle: "閒置", done: "完成", fail: "失敗", danger: "注意" };

// replay：session 已不在 live（#653），從最新的往前讀（beforePos 不帶）
function paneMore(replay = false) {
  if (paneMoreReq || ui.tab === "all") return;
  const beforePos = replay ? null : pane.stream.minPos().get(pane.session);
  if (!replay && !(beforePos > 1)) return;
  paneMoreReq = { session: pane.session, beforePos, timer: setTimeout(() => { paneMoreReq = null; }, 3000) };
  client.more({ session: pane.session, beforePos, limit: 200 });
}
function onPaneMore(m) {
  clearTimeout(paneMoreReq.timer);
  paneMoreReq = null;
  paneView.prepended(pane.prepend(m.events));
  paneHasMore();
}
function paneHasMore() {
  paneView.setHasMore((pane.stream.minPos().get(pane.session) ?? 1) > 1);
}

function lane(a) {
  const isBg = a.agentId === "bg";
  return h("div", { class: `lane ${a.status}${isBg ? " bg" : ""}`, title: [a.label, a.agentType, a.model && (a.modelId ?? a.model), a.task].filter(Boolean).join("\n") },
    h("span", { class: "nm" }, h("span", {}, a.label), a.model ? h("small", { title: a.modelId ?? a.model }, a.model) : null),
    h("span", { class: `stt ${a.status}` }, LANE_TEXT[a.status] ?? a.status, a.toolCount ? ` · ${a.toolCount} 個工具` : ""),
    h("span", { class: "cur" }, a.current ?? "—"),
    a.task ? h("span", { class: "tsk" }, a.task.replace(/\s+/g, " ")) : null);
}

function renderPane() {
  const on = ui.tab !== "all";
  $("#allblk").hidden = on;
  $("#spane").hidden = !on;
  if (!on) return;
  const id = pane.session;
  // 已從 live 移除的 session（回放）只有 /api/sessions 補進來的列
  const s = store.sessions.get(id) ?? left.fetched.find((r) => r.id === id);
  const v = agentView([...store.agents.values()], id, { sessionModel: s?.model ?? null });
  const main = store.agents.get(`${id}/main`);
  const model = s?.model ?? main?.model ?? null; // sessions.model 還沒有值時先用 main agent 的
  const modelId = s?.model ? s.modelId : main?.modelId; // 完整 id 放滑過提示（#643）
  const g = pane.git;
  const now = nowOf(v, s);

  if (changed("spHead", JSON.stringify([id, s?.lastPrompt, s?.firstPrompt, s?.doing, model, modelId, s?.branch, s?.cwd, s?.commits, s?.dangers, s?.costUsd, g.last, g.commits.length]))) {
    $("#spDoing").hidden = !s?.doing;
    $("#spDoing").textContent = s?.doing ? `▸ ${s.doing}` : "";
    $("#spSid").textContent = shortId(id);
    $("#spSid").style.color = sessionColor(id);
    $("#spSid").title = id;
    $("#spPrompt").textContent = s?.lastPrompt ?? s?.firstPrompt ?? "";
    $("#spPrompt").title = s?.firstPrompt ? `第一個 prompt：${s.firstPrompt}` : "";
    $("#spModel").textContent = model ?? "";
    $("#spModel").title = modelId ?? model ?? "";
    $("#spBranch").textContent = g.last?.branch ?? s?.branch ?? "—";
    $("#spCwd").textContent = s?.cwd ?? "—";
    $("#spCommits").textContent = String(Math.max(s?.commits ?? 0, g.commits.length)); // collector 的計數為準，看到的 commit 比它多時不倒退
    $("#spLast").textContent = g.last
      ? `最後 ${String(g.last.hash ?? "").slice(0, 7)} ${g.last.message ?? ""}（${agentWho(id, g.last.agent, agentName)}）`
      : s?.commits ? "commit 在目前載入的範圍之前" : "尚無 commit";
    $("#spLast").classList.toggle("pick", !!g.last?.hash);
    $("#spLast").title = g.last?.hash ? "點一下：左欄的改動改成相對這個 commit" : "";
    $("#spDanger").hidden = !s?.dangers;
    $("#spDangerN").textContent = String(s?.dangers ?? 0);
    $("#spCost").textContent = s?.costUsd != null ? fmtKpi("costUsd", s.costUsd) : "—";
  }
  if (changed("spNow", JSON.stringify(now))) {
    $("#spNow").className = `now ${now.state}`;
    $("#spNowSt").textContent = NOW_TEXT[now.state] ?? now.state;
    $("#spNowWhat").textContent = now.what;
    $("#spNowWhat").title = now.what;
    $("#spNowAg").textContent = now.agent;
  }
  const th = pane.thinking;
  if (changed("spThink", JSON.stringify(th))) {
    $("#spThink").hidden = !th;
    if (th) {
      $("#spThinkText").textContent = th.text.replace(/\s+/g, " ");
      $("#spThinkText").title = th.text;
      $("#spThinkTag").textContent = `${agentWho(id, th.agent, agentName)}${th.summarized ? " · 摘要" : ""}`;
    }
  }
  renderPaneDanger(id);
  renderPaneFiles(id);
  // 開出來的 agent：全部列出（執行中與出事的在前，已完成的依完成時間），main 與背景放最前面
  const lanes = [v.main, v.bg, ...v.active, ...v.done].filter(Boolean);
  if (changed("spLanes", JSON.stringify(lanes.map((a) => [a.id, a.status, a.label, a.model, a.current, a.toolCount])))) {
    $("#spLanes").replaceChildren(...(lanes.length ? lanes.map(lane) : [h("div", { class: "none" }, "尚無 agent")]));
    $("#spAgN").textContent = `subagent 執行中 ${v.running} · 完成 ${v.done.length}`;
  }
}

// 危險指令提示框：這個 session 還沒確認的危險列；工具已結束（clearedAt）的變淡
function renderPaneDanger(id) {
  const rows = sessionDangers([...store.attention.values()], id);
  const canAck = ui.welcome?.scope !== "view";
  if (!changed("spDz", `${Math.floor(Date.now() / 30_000)}|${canAck}|` + rows.map((r) => `${r.id}:${r.clearedAt ?? ""}:${ackPending.has(r.id)}`).join(","))) return;
  $("#spDz").hidden = !rows.length;
  $("#spDzN").textContent = rows.length ? `${rows.length} 筆待確認` : "";
  const now = Date.now();
  $("#spDzList").replaceChildren(...rows.map((r) => {
    const pend = ackPending.has(r.id);
    const ack = { class: "btn sm", onclick: () => ackRow(r) };
    if (pend) ack.disabled = "";
    return h("div", { class: `dz${r.clearedAt != null ? " cleared" : ""}${pend ? " pend" : ""}` },
      h("span", { class: "ar" }, r.reason ?? ""),
      h("span", { class: "at" }, r.clearedAt != null ? "已結束" : `執行中 · ${ago(now - (r.raisedAt ?? now))}`),
      canAck ? h("button", ack, pend ? "確認中…" : "已確認") : null);
  }));
}

// 正在修改的檔案：這個 session 碰過的每個檔案一列（session-pane.js 的 files()）；點一列看它最近一次的 diff
function renderPaneFiles(id) {
  const files = pane.files();
  const ch = pane.changes;
  // 還沒手動點過：跟著清單第一列（正在改的、或最近改的那個檔案）；點過就停在那一列，跟改動紀錄的規則一樣
  if (ch.following && files.length) ch.selectedKey = `${id}:${files[0].lastSeq}`;
  const sel = ch.selected();
  if (!changed("spFiles", `${id}|${sel?.key ?? ""}|` + files.map((f) => `${f.path}:${f.edits}:${f.writes}:${f.running}:${f.lastSeq}`).join(","))) return;
  const who = (a) => agentWho(id, a, agentName);
  $("#spFilesN").textContent = files.length ? `${files.length} 個檔案${files.some((f) => f.running) ? "，正在改的在最上面" : ""}` : "這個 session 碰過的檔案";
  $("#spFiles").replaceChildren(...(files.length ? files.map((f) => h("button", {
    class: `${f.running ? "run" : ""}${sel?.path === f.path ? " sel" : ""}`, title: `${f.path}\n${f.agents.map(who).join("、")}`,
    "aria-pressed": String(sel?.path === f.path),
    onclick: () => {
      ch.select(`${id}:${f.lastSeq}`);
      ch.following = f === files[0]; // 點回第一列就恢復自動跟隨
      render(true);
    },
  },
  h("i", { class: "fdot" }),
  h("span", { class: "pth" }, f.path),
  f.isNew ? h("span", { class: "k w" }, "新") : null,
  h("span", { class: "n" }, f.edits ? h("span", {}, `改 ${f.edits}`) : null, f.writes ? h("span", {}, ` 寫 ${f.writes}`) : null,
    f.added || f.removed ? h("span", {}, " ", h("span", { class: "a" }, `+${f.added}`), " ", h("span", { class: "d" }, `−${f.removed}`)) : null),
  h("span", { class: "ag" }, f.agents.map(who).join("、")))) : [h("div", { class: "none" }, "尚無改動")]));
  $("#spFb").hidden = !sel;
  if (sel) {
    $("#spFbKind").textContent = sel.kind === "edit" ? `EDIT −${sel.removed} +${sel.added} · ${who(sel.agent)}` : `WRITE${sel.isNew ? "（新檔案）" : ""} · ${who(sel.agent)}`;
    $("#spFbPath").textContent = sel.path;
    $("#spFbBody").innerHTML = diffHtml(sel);
  }
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
        h("span", { class: "ag" }, agentWho(c.session, c.agent, agentName)));
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
  const who = `${shortId(c.session)} · ${agentWho(c.session, c.agent, agentName)}`;
  $("#fbKind").textContent = c.kind === "edit" ? `EDIT −${c.removed} +${c.added} · ${who}` : `WRITE${c.isNew ? "（新檔案）" : ""} · ${who}`;
  $("#fbPath").textContent = c.path;
  $("#fbPath").title = c.path;
  $("#fbBody").innerHTML = diffHtml(c);
  $("#gfb").scrollTop = 0;
}

// 修改情形的內容（全部串流的右欄與單 session 的檔案變更共用）
function diffHtml(c) {
  const rows = c.kind === "edit" ? diffRows(c.hunk, c.hunkSource) : writeRows(c.content, c.truncated);
  const no = (v) => (v == null ? "" : String(v));
  return rows.map((r) => {
    if (r.type === "hunk") return `<div class="hunk">${esc(r.text)}</div>`;
    if (r.type === "note") return `<div class="note">${esc(r.text)}</div>`;
    if (r.type === "line") return `<div class="dl w"><span class="no">${r.new}</span><span class="tx">${esc(r.text)}</span></div>`;
    const mk = r.type === "add" ? "+" : r.type === "del" ? "−" : " ";
    return `<div class="dl ${r.type}"><span class="no">${no(r.old)}</span><span class="no">${no(r.new)}</span><span class="mk">${mk}</span><span class="tx">${esc(r.text)}</span></div>`;
  }).join("");
}

function selectChange(key) {
  changes.select(key);
  render(true);
}

// ---------- 合流串流 ----------
const stream = createStream();
// 串流前綴、改動紀錄、Agents 面板用同一套名稱（agents.js）
const agentName = (session, agentId) => { const a = store.agents.get(`${session}/${agentId}`); return a ? agentLabel(a) : null; };
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

const paneView = createStreamView({
  scroller: $("#spStream"),
  model: pane.stream,
  agentName,
  onNeedMore: paneMore,
  onFollow: (on) => {
    if (on && pane.stream.holding) paneView.update({ ...pane.stream.trim(), appended: 0, updated: new Set() });
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
  if (id === ui.tab) return;
  switchTab(client, ui.tab, id);
  ui.tab = id;
  if (id !== "all") {
    // 先清空，等 session 頻道的 snapshot 進來再畫（subscribe 一定會回 snapshot，contracts §6）
    pane.open(id);
    paneMoreReq = null;
    paneView.reset();
    paneView.setHasMore(false);
  }
  render(true);
}

for (const t of document.querySelectorAll(".tab")) t.addEventListener("click", () => showLeftTab(t.dataset.t));

// ---------- 連線 ----------
const params = new URLSearchParams(location.search);
const mockN = params.has("mock") ? Math.max(1, Math.min(32, Number(params.get("mock")) || 12)) : 0;
bench.on = mockN > 0 && params.has("bench");
bench.isolate = bench.on && params.has("isolate");
// &prefill：mock 先產生一段歷史（每個 session 約 220 個 pos），用來試「載入更多」與 3000 行上限
// &bigtree：檔案樹超過懶載入門檻（#642）
const mock = mockN ? installMock(mockN, { bench: bench.on, prefill: params.has("prefill"), bigtree: params.has("bigtree") }) : null;
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
    welcome: (m) => { ui.welcome = m; notifier.connected(); loadSessions(); render(true); },
    snapshot: (m) => {
      // 監視器沒有 `*`：彙總列（這個 session 的一列、agents、attention）也從 session 頻道來
      if (isMonitor && m.ch === `session:${monitorId}`) applySnapshot(store, m);
      if (ui.tab !== "all" && m.ch === `session:${pane.session}`) {
        pane.load({ staged: m.staged, stream: m.stream });
        paneMoreReq = null;
        paneView.reset();
        paneHasMore();
        if (needsReplay(pane.session, m)) paneMore(true);
      }
      if (m.ch === "*") {
        applySnapshot(store, m);
        notifier.snapshot(m.attention);
        onStreamSnapshot(m);
        changes.load(m.changes);
        syncProjects();
        feedFiles([...(m.stream ?? []), ...(m.changes ?? []), ...(m.staged ?? [])]);
      }
      render(true);
    },
    frame: (m) => {
      applyFrame(store, m);
      if (m.ch === "*") notifier.frame(m.rows?.attention);
      // 單 session 畫面只吃自己的頻道（`*` 也帶同樣的 Event，兩邊都吃會重複算）
      if (ui.tab !== "all" && m.ch === `session:${pane.session}`) paneView.update(pane.apply(m.events));
      // session:<id> 頻道的 Event 與 `*` 重複，只有 `*` 進合流串流
      if (m.ch === "*") {
        if (bench.on) benchFrame(m);
        else onStreamFrame(m);
        changes.apply(m.events);
        if (m.rows?.sessions?.length) syncProjects();
        feedFiles(m.events);
      }
      render();
    },
    more: (m) => (paneMoreReq && m.session === paneMoreReq.session && m.beforePos === paneMoreReq.beforePos ? onPaneMore(m) : onMore(m)),
    onStatus: (s) => { if (ui.conn !== "incompatible") ui.conn = s; render(true); },
    onIncompatible: () => { ui.conn = "incompatible"; render(true); },
    onError: (e) => console.warn("ws message", e),
  },
});
if (isMonitor) {
  document.body.classList.add("monitor");
  document.title = `監視 ${shortId(monitorId)} · Coding Lab`;
  pane.open(monitorId);
}
// 改 hash（主視窗 ↔ 監視器、換 session）時整頁重來，訂閱與畫面都從頭建立
addEventListener("hashchange", () => location.reload());
for (const ch of channelsFor(monitorId)) client.subscribe(ch);
client.connect();
render();
