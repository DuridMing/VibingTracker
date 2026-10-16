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
import { agentView, agentLabel } from "./agents.js";
import { createSessionPane, nowOf, switchTab } from "./session-pane.js";
import { parseMonitor, channelsFor, monitorUrl } from "./monitor.js";
import { loadSettings, saveSettings, sinksFor, summarize, createNotifier, createLeader } from "./notify.js";

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
          h("span", { class: "m" }, r.model),
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
  if (!changed("tabs", ui.tab + "|" + live.map((s) => `${s.id}:${s.status}:${s.lastPrompt ?? ""}`).join("|"))) return;
  box.replaceChildren(
    h("button", { class: `stab${ui.tab === "all" ? " on" : ""}`, onclick: () => selectTab("all") }, "全部串流 ", h("small", {}, String(live.length))),
    ...live.map((s) =>
      h("button", { class: `stab ${s.status}${ui.tab === s.id ? " on" : ""}`, title: `${s.id}\n${s.lastPrompt ?? ""}`, onclick: () => selectTab(s.id) },
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

// 每個 session 一列：main、背景、執行中與出事的 subagent 各一個 chip；已完成的收成「✓ n」（contracts.md §6）。
function agentChip(a) {
  const isBg = a.agentId === "bg";
  const title = [a.label, a.agentType, a.model, a.task, AGENT_STATUS_TEXT[a.status], a.current].filter(Boolean).join("\n");
  return h("span", { class: `agc ${a.status}${isBg ? " bg" : ""}`, title },
    h("i"), h("b", {}, a.label),
    a.model ? h("small", { class: "mdl" }, a.model) : null,
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
    const sig = chips.map((a) => `${a.id}:${a.status}:${a.label}:${a.model ?? ""}:${a.current ?? ""}`).join("|") + `|done:${v.done.map((a) => a.id).join(",")}`;
    let row = agentRows.get(s.id);
    if (!row || row.sig !== sig) {
      const parts = chips.map(agentChip);
      if (v.done.length) parts.push(doneChip(v.done));
      const el = h("div", { class: "agrow" }, sessTag(s, "ss"), h("span", { class: "ags" }, ...(parts.length ? parts : [h("span", { class: "none" }, "—")])));
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

// ---------- 單 session 畫面（#649）：tab 切到某個 session 時取代全部串流那一塊 ----------
// 資料只來自 `session:<id>` 頻道（session-pane.js）；NOW 與 lanes 用 collector 的 agents 列，跟 Agents 面板同一份。
const pane = createSessionPane();
let paneMoreReq = null;
const NOW_TEXT = { run: "執行中", wait: "等你", done: "完成", fail: "失敗", danger: "危險", idle: "閒置", ended: "結束" };
const LANE_TEXT = { run: "執行中", idle: "閒置", done: "完成", fail: "失敗", danger: "注意" };

function paneMore() {
  if (paneMoreReq || ui.tab === "all") return;
  const beforePos = pane.stream.minPos().get(pane.session);
  if (!(beforePos > 1)) return;
  paneMoreReq = { session: pane.session, beforePos, timer: setTimeout(() => { paneMoreReq = null; }, 3000) };
  client.more({ session: pane.session, beforePos, limit: 200 });
}
function onPaneMore(m) {
  clearTimeout(paneMoreReq.timer);
  paneMoreReq = null;
  paneView.prepended(pane.stream.prepend([m.events ?? []]));
  paneHasMore();
}
function paneHasMore() {
  paneView.setHasMore((pane.stream.minPos().get(pane.session) ?? 1) > 1);
}

function lane(a) {
  const isBg = a.agentId === "bg";
  return h("div", { class: `lane ${a.status}${isBg ? " bg" : ""}`, title: [a.label, a.agentType, a.model, a.task].filter(Boolean).join("\n") },
    h("span", { class: "nm" }, h("span", {}, a.label), a.model ? h("small", {}, a.model) : null),
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
  const s = store.sessions.get(id);
  const v = agentView([...store.agents.values()], id, { sessionModel: s?.model ?? null });
  const model = s?.model ?? store.agents.get(`${id}/main`)?.model ?? null; // sessions.model 還沒有值時先用 main agent 的
  const g = pane.git;
  const now = nowOf(v, s);

  if (changed("spHead", JSON.stringify([id, s?.lastPrompt, s?.firstPrompt, model, s?.branch, s?.cwd, s?.commits, s?.dangers, s?.costUsd, g.last, g.commits.length]))) {
    $("#spSid").textContent = shortId(id);
    $("#spSid").style.color = sessionColor(id);
    $("#spSid").title = id;
    $("#spPrompt").textContent = s?.lastPrompt ?? s?.firstPrompt ?? "";
    $("#spPrompt").title = s?.firstPrompt ? `第一個 prompt：${s.firstPrompt}` : "";
    $("#spModel").textContent = model ?? "";
    $("#spBranch").textContent = g.last?.branch ?? s?.branch ?? "—";
    $("#spCwd").textContent = s?.cwd ?? "—";
    $("#spCommits").textContent = String(Math.max(s?.commits ?? 0, g.commits.length)); // collector 的計數為準，看到的 commit 比它多時不倒退
    $("#spLast").textContent = g.last
      ? `最後 ${String(g.last.hash ?? "").slice(0, 7)} ${g.last.message ?? ""}（${g.last.agent === "main" ? "main" : agentName(id, g.last.agent) ?? shortId(g.last.agent)}）`
      : s?.commits ? "commit 在目前載入的範圍之前" : "尚無 commit";
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
      $("#spThinkTag").textContent = `${th.agent === "main" ? "main" : agentName(id, th.agent) ?? shortId(th.agent)}${th.summarized ? " · 摘要" : ""}`;
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
  const who = (a) => (a === "main" ? "main" : a === "bg" ? "背景" : agentName(id, a) ?? shortId(a));
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
    welcome: (m) => { ui.welcome = m; notifier.connected(); render(true); },
    snapshot: (m) => {
      // 監視器沒有 `*`：彙總列（這個 session 的一列、agents、attention）也從 session 頻道來
      if (isMonitor && m.ch === `session:${monitorId}`) applySnapshot(store, m);
      if (ui.tab !== "all" && m.ch === `session:${pane.session}`) {
        pane.load({ staged: m.staged, stream: m.stream });
        paneMoreReq = null;
        paneView.reset();
        paneHasMore();
      }
      if (m.ch === "*") {
        applySnapshot(store, m);
        notifier.snapshot(m.attention);
        onStreamSnapshot(m);
        changes.load(m.changes);
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
