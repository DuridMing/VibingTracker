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
import { sortAttention, sessionDangers, ackAllTargets, KIND_TEXT } from "./attention.js";
import { agentView, agentLabel, agentWho } from "./agents.js";
import { createSessionPane, nowOf, switchTab, sessionTitle, snapshotLoad, isSessionTab } from "./session-pane.js";
import { REPLAY_TAB, rangeFor, replayUrl, createReplay } from "./replay.js";
import { parseMonitor, channelsFor, monitorUrl } from "./monitor.js";
import { loadSettings, saveSettings, sinksFor, summarize, createNotifier, createLeader } from "./notify.js";
import { createFileTree, filesUrl, sinceChanges } from "./tree.js";
import { mergeSessions, projectRoots, canReplay, fitTabs } from "./sessions.js";
import { projectColors, projectColor, projectLabels, projectOf, sessionFilterFor, groupByProject, projectTreeRows } from "./projects.js";
import { degradedNotice } from "./degraded.js";
import { branchesUrl, branchDiffUrl, graphUrl, commitUrl, commitDiffUrl, changedBranches, diffLines, worktreeLabel, graphNotices, layoutGraph, withWip } from "./branches.js";

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
const ui = { tab: monitorId ?? "all", conn: "connecting", welcome: null, replayNext: null, ackError: null }; // replayNext：按了 ▶ 回放、等 snapshot 的 session；ackError：上一次 ack 被 server 拒絕的原因（#721）

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

// 專案（#669）：proj.cur 是單一專案的根 cwd，null＝全部專案。單一專案時其他專案的列只隱藏、不移除
function inProject(id) {
  return !proj.cur || inProjectCwd(cwdOf(id));
}
// 還不知道 cwd 的 session（WS 列還沒到）不藏，免得漏看
function inProjectCwd(cwd) {
  return !proj.cur || !cwd || projectOf(cwd, proj.rootCwds) === proj.cur;
}
function projectTag(id) {
  if (!proj.many) return null;
  const p = projectOf(cwdOf(id), proj.rootCwds);
  return p ? { label: proj.labels.get(p) ?? p, color: projectColor(proj.colors, p), cwd: p } : null;
}
// hub（多機總覽，#663）：welcome.host 是 hub 時每行標 [host]（sessions 列的 host 是上游名稱）
function hostTag(id) {
  if (ui.welcome?.host !== "hub") return null;
  return store.sessions.get(id)?.host ?? fetchedIndex.get(id)?.host ?? null;
}
function hostChip(id) {
  const t = hostTag(id);
  return t ? h("span", { class: "hst", title: t }, `[${t}]`) : null;
}
const statusText = (s) => (s.offline ? "離線" : STATUS_TEXT[s.status] ?? s.status ?? "");

function pjChip(id) {
  const t = projectTag(id);
  return t ? h("span", { class: "pj", style: `color:${t.color}`, title: t.cwd }, t.label) : null;
}

function renderProjectSwitch() {
  const sel = $("#gProj");
  if (!changed("gProj", JSON.stringify([proj.cur, proj.roots.map((r) => [r.cwd, r.live, r.sessions.length])]))) return;
  sel.hidden = isMonitor || (proj.roots.length < 2 && !proj.cur);
  const live = proj.roots.reduce((n, r) => n + r.live, 0);
  sel.replaceChildren(
    h("option", { value: "" }, `全部專案（${proj.roots.length}）· ${live} 個進行中`),
    ...proj.roots.map((r) => h("option", { value: r.cwd, title: r.cwd }, `${proj.labels.get(r.cwd) ?? r.cwd}（${r.live}/${r.sessions.length}）`)),
  );
  sel.value = proj.cur ?? "";
  sel.style.color = proj.cur ? projectColor(proj.colors, proj.cur) : "";
  sel.title = proj.cur ? `只看 ${proj.cur}\n其他專案的 session 只是隱藏，切回「全部專案」就回來` : "專案：全部（串流每行標專案、檔案樹與改動紀錄依專案分組）或只看一個";
}

function sessionTab(s, extra = {}) {
  const host = hostTag(s.id);
  return h("button", { class: `stab ${s.status}${s.offline ? " offline" : ""}${ui.tab === s.id ? " on" : ""}`, title: `${host ? `[${host}]${s.offline ? "（離線）" : ""}\n` : ""}${sessionTitle(s)}`, onclick: () => selectTab(s.id), ...extra },
    h("span", { class: "sw", style: `background:${sessionColor(s.id)}` }),
    h("span", { class: "dot" }),
    host ? h("small", { class: "hst" }, host) : null,
    sessTag(s),
    h("span", { class: "pop", role: "button", tabindex: "0", title: "開成監視器視窗（可拖到第二螢幕，關掉主視窗也能繼續看）", onclick: (e) => { e.stopPropagation(); openMonitor(s.id); } }, "⧉"));
}

// 放不下的 session tab 收進「+N」（#704）：先全部畫出來量寬，再依 fitTabs 收起來；寬度變了重算
let tabsHidden = [];
function renderTabs() {
  const box = $("#stabs");
  const live = liveSessions().filter((s) => inProject(s.id));
  if (!changed("tabs", ui.tab + "|" + proj.cur + "|" + (ui.welcome?.host ?? "") + "|" + live.map((s) => `${s.id}:${s.status}:${s.offline ? "off" : ""}:${s.host}:${s.lastPrompt ?? ""}:${s.doing ?? ""}`).join("|"))) return;
  box.replaceChildren(
    h("button", { class: `stab${ui.tab === "all" ? " on" : ""}`, onclick: () => selectTab("all") }, "全部串流 ", h("small", {}, String(live.length))),
    ...live.map((s) => sessionTab(s, { "data-sid": s.id })),
    h("button", { class: "stab more", id: "stMore", "aria-haspopup": "true", "aria-expanded": "false", onclick: (e) => { e.stopPropagation(); toggleTabMenu(); } }, "+99"),
    h("button", { class: `stab rp${ui.tab === REPLAY_TAB ? " on" : ""}`, title: "選專案與時間範圍，回放所有 session（含已結束）合成的串流", onclick: () => selectTab(REPLAY_TAB) }, "⟲ 專案回放"),
  );
  fitTabRow();
}

function fitTabRow() {
  const box = $("#stabs");
  const tabs = [...box.querySelectorAll(".stab[data-sid]")];
  const more = $("#stMore");
  if (!more) return;
  for (const t of tabs) t.hidden = false;
  more.hidden = false;
  more.textContent = "+99";
  const gap = 4;
  const first = box.firstElementChild;
  const replay = box.querySelector(".stab.rp"); // 「⟲ 專案回放」（#702）永遠留在最後
  const avail = box.clientWidth - first.offsetWidth - gap - (replay ? replay.offsetWidth + gap : 0);
  const sel = tabs.findIndex((t) => t.dataset.sid === ui.tab);
  const { hidden } = fitTabs(tabs.map((t) => t.offsetWidth + gap), avail, more.offsetWidth + gap, sel);
  for (const i of hidden) tabs[i].hidden = true;
  tabsHidden = hidden.map((i) => tabs[i].dataset.sid);
  more.hidden = !hidden.length;
  more.textContent = `+${hidden.length}`;
  more.title = `還有 ${hidden.length} 個 session，點一下列出來`;
  if (!hidden.length) closeTabMenu();
  else if (!$("#tabMenu").hidden) renderTabMenu();
}
if (typeof ResizeObserver === "function") new ResizeObserver(() => fitTabRow()).observe($("#stabs"));

function renderTabMenu() {
  const byId = new Map(liveSessions().map((s) => [s.id, s]));
  const menu = $("#tabMenu");
  menu.replaceChildren(
    h("div", { class: "tmh" }, `收起來的 session（${tabsHidden.length}）`),
    ...tabsHidden.map((id) => byId.get(id)).filter(Boolean).map((s) => sessionTab(s, { onclick: () => { closeTabMenu(); selectTab(s.id); } })),
  );
  const b = $("#stMore").getBoundingClientRect();
  menu.style.top = `${b.bottom + 4}px`;
  menu.style.left = `${Math.max(8, Math.min(b.right - menu.offsetWidth, window.innerWidth - menu.offsetWidth - 8))}px`;
}
function toggleTabMenu() {
  if (!$("#tabMenu").hidden) return closeTabMenu();
  $("#tabMenu").hidden = false;
  $("#stMore")?.setAttribute("aria-expanded", "true");
  renderTabMenu();
}
function closeTabMenu() {
  $("#tabMenu").hidden = true;
  $("#stMore")?.setAttribute("aria-expanded", "false");
}
document.addEventListener("click", (e) => { if (!$("#tabMenu").hidden && !$("#tabMenu").contains(e.target)) closeTabMenu(); });
document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeTabMenu(); });

function openMonitor(id) {
  window.open(monitorUrl(location, id), `coding-lab-monitor-${id}`, "popup,width=980,height=760");
}

function renderSessFilters() {
  // 進行中的 session，加上被隱藏的（即使已結束，也要能點回來顯示）
  const live = [...store.sessions.values()].filter((s) => (s.status !== "ended" || stream.hiddenSessions.has(s.id)) && inProject(s.id));
  if (!changed("fSess", proj.cur + "|" + live.map((s) => `${s.id}:${s.status === "ended"}:${stream.hiddenSessions.has(s.id)}`).join("|"))) return;
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
  const staged = [...stream.staged.values()].filter((c) => inProject(c.session));
  if (changed("liveN", String(staged.length))) $("#liveN").textContent = String(staged.length);
  const list = $("#liveList");
  if (!liveWin.isOpen || !changed("liveList", `${proj.many}|` + staged.slice(-100).map((c) => `${c.toolUseId}:${c.status}`).join("|"))) return;
  $("#liveWinTitle").textContent = `進行中的動作（${staged.length}）`;
  const cards = staged.slice(-100);
  list.replaceChildren(
    ...(cards.length
      ? cards.map((c) => {
          const f = formatRow({ key: c.toolUseId, session: c.session, agent: c.agent, type: "tool", card: c, v: 0, expanded: false, delayed: false }, { agentName, projectTag, hostTag });
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
  const all = allSessions().filter((s) => inProject(s.id));
  if (!changed("sess", `${ui.tab}|${proj.cur}|${proj.many}|${ui.welcome?.host ?? ""}|` + all.map((s) => `${s.id}:${s.status}:${s.offline ? "off" : ""}:${s.host}:${s.firstPrompt}:${s.branch}:${s.commits}:${s.dangers}:${s.model}:${s.doing ?? ""}`).join("|"))) return;
  $("#sessCount").textContent = all.length ? String(all.length) : "";
  $("#sess").replaceChildren(
    ...(all.length
      ? all.slice(0, SESS_SHOWN).map((s) =>
          h("div", { class: "srow" },
            h("button", { class: `${ui.tab === s.id ? "sel" : ""}${s.status === "ended" ? " ended" : ""}`, title: sessionTitle(s), onclick: () => selectTab(s.id) },
              sessTag(s),
              h("span", {}, h("span", { class: "pr" }, s.firstPrompt ?? "—"),
                s.doing && s.status !== "ended" ? h("small", { class: "doing" }, `▸ ${s.doing}`) : null,
                h("small", {}, hostChip(s.id), pjChip(s.id), `⎇ ${s.branch ?? "—"} · commits ${s.commits ?? 0}${s.dangers ? ` · ⚠ ${s.dangers}` : ""}`, s.model ? h("span", { class: "mdl", title: s.modelId ?? s.model }, s.model) : null)),
              h("span", { class: `r ${s.offline ? "offline" : s.status}` }, statusText(s))),
            canReplay(s) ? h("button", { class: "btn sm rp", title: "從存下來的紀錄重新讀回這個 session", onclick: () => replaySession(s.id) }, "▶ 回放") : null))
      : [h("div", { class: "none" }, proj.cur && store.sessions.size ? "這個專案沒有 session" : "尚無 session")]),
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
  const live = liveSessions().filter((s) => inProject(s.id));
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
      const el = h("div", { class: `agrow${s.offline ? " offline" : ""}`, title: sessionTitle(s) }, hostChip(s.id), sessTag(s, "ss"),
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
    renderProjectSwitch();
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
    renderBranches();
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
  const allRows = sortAttention([...store.attention.values()], scoreOf);
  const rows = allRows.filter((r) => inProject(r.session));
  const elsewhere = allRows.length - rows.length; // 單一專案時其他專案的件數，只提示不列
  const now = Date.now();
  const canAck = ui.welcome?.scope !== "view";
  const sig = `${Math.floor(now / 30_000)}|${canAck}|${ui.tab}|${proj.cur}|${elsewhere}|${ui.ackError ?? ""}|` + rows.map((r) => `${r.id}:${scoreOf(r.session) ?? 0}:${ackPending.has(r.id)}:${r.reason}:${r.clearedAt ?? ""}`).join(",");
  if (!changed("att", sig)) return;
  const todo = canAck ? ackAllTargets(rows, ackPending, scoreOf) : [];
  $("#ackAll").hidden = !todo.length;
  $("#ackAll").textContent = `全部確認（${todo.length}）`;
  const box = $("#att");
  const other = elsewhere ? h("div", { class: "none" }, `其他專案還有 ${elsewhere} 件（切回「全部專案」看）`) : null;
  // server 拒絕上一次「已確認」（hub 上那台是 view 金鑰）：顯示原因，下一次按就清掉（#721）
  const err = ui.ackError ? h("div", { class: "none err", role: "alert" }, `⚠ ${ui.ackError}`) : null;
  if (!rows.length) {
    box.replaceChildren(err ?? "", h("div", { class: "none" }, elsewhere ? "這個專案沒有需要處理的 session" : "目前沒有需要處理的 session"), other ?? "");
    return;
  }
  box.replaceChildren(err ?? "", ...rows.map((r) => {
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
  }), other ?? "");
}

function ackRow(r) {
  ui.ackError = null;
  ackPending.add(r.id);
  client.ack({ session: r.session, kind: r.kind, ref: r.ref });
  render(true);
}

// server 回 error（目前只有 ack 會被拒：那台上游是 view 金鑰）：解除「確認中…」、把原因放在佇列上方（#721）
function onServerError(m) {
  ui.ackError = m?.message || "操作被拒";
  ackPending.clear();
  render(true);
}

// 全部確認（#697）：每列各送一次既有的 ack，不改 WS 協定
$("#ackAll").addEventListener("click", () => {
  ui.ackError = null;
  for (const r of ackAllTargets([...store.attention.values()].filter((r) => inProject(r.session)), ackPending, scoreOf)) {
    ackPending.add(r.id);
    client.ack({ session: r.session, kind: r.kind, ref: r.ref });
  }
  render(true);
});

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
$("#spReplay").addEventListener("click", () => replaySession(pane.session));
setInterval(() => render(), 30_000); // 佇列的「幾分前」

// ---------- 左欄：專案檔案（#642）與改動基準（#644） ----------
// 樹與 git 狀態來自 GET /api/files（tree.js 的 filesUrl）；方塊來自 `*` 頻道的 file.* Event（tree.js 累積）。
// 「本次開啟以來」不問 collector：用這個頁面收到的 Event 累積（sinceChanges）。
// 專案：session 的 cwd 歸組（sessions.js）；全部專案時每個專案一棵樹疊著看（標題列可收起），單一專案時只看那棵。
// Event 先收在 fileEvents，開新樹時重放。
const ROW_H = 22;
const FILE_EVENTS_MAX = 50_000;
const BASE_TEXT = { main: "相對整合分支", since: "本次開啟以來", session: "單一 session", commit: "相對 commit" };
const pref = {
  get: (k) => { try { return storage?.getItem(`coding-lab.tree.${k}`) ?? null; } catch { return null; } },
  set: (k, v) => { try { storage?.setItem(`coding-lab.tree.${k}`, v); } catch { /* 無痕模式等 */ } },
};
const left = {
  tab: "tree",
  fetched: [], // GET /api/sessions 的列
  base: BASE_TEXT[pref.get("base")] ? pref.get("base") : "main",
  baseSess: null,
  hash: "",
  hashCwd: null, // 從單 session 的 git 列點 commit 時，用那個 session 的 worktree
  trees: new Map(), // 專案 cwd → { tree, loading, error, info, req }
  fileEvents: [],
  dirty: true,
  rows: [],
  els: new Map(), // `${cwd}|${path}`（專案標題列是 `#${cwd}`）→ { el, sig }
  flash: new Map(), // `${cwd}|${path}` → 閃爍的 class（下次畫時套上）
  lit: new Set(), // 被點亮的父目錄（同樣的鍵）
  refetch: 0,
};
// 專案切換（#669）：cur 是單一專案的根 cwd，null＝全部專案（預設）；roots 來自 sessions.js 的 projectRoots
const proj = { cur: pref.get("project") || null, roots: [], rootCwds: [], colors: new Map(), labels: new Map(), many: false, collapsed: new Set(), sig: "" };
const activeRoots = () => (proj.cur ? [proj.cur] : proj.rootCwds);
const fetchedById = () => new Map(left.fetched.map((s) => [s.id, s]));
let fetchedIndex = new Map();
const cwdOf = (id) => store.sessions.get(id)?.cwd ?? fetchedIndex.get(id)?.cwd ?? null;
// 401：cookie 過期或金鑰被撤銷（#667），整頁導去登入
const toLogin = (r) => { if (r.status === 401) location.assign("/auth/login"); return r; };
const apiFetch = (url) => (mock ? mock.fetch(url) : fetch(url, { credentials: "same-origin" }).then(toLogin));
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
    ent = { tree, loading: false, error: null, info: "", req: 0 };
    left.trees.set(cwd, ent);
  }
  return ent;
}

// 專案清單變了（新 session、Sessions 補進來的）：更新色與名稱、串流的專案篩選；選的專案不見了就回到全部；還沒載入的樹去抓
function syncProjects() {
  proj.roots = projectRoots(allSessions());
  proj.rootCwds = proj.roots.map((r) => r.cwd);
  if (proj.cur && !proj.rootCwds.includes(proj.cur)) proj.cur = null;
  proj.many = !proj.cur && proj.rootCwds.length > 1;
  const sig = `${proj.cur}|${proj.rootCwds.join("|")}`;
  if (sig !== proj.sig) {
    proj.sig = sig;
    proj.colors = projectColors(proj.rootCwds);
    proj.labels = projectLabels(proj.rootCwds);
    stream.setSessionFilter(sessionFilterFor(proj.cur, proj.rootCwds, cwdOf));
    view.filterChanged(); // 專案色標與可見性都變了，整批重畫
    left.dirty = true;
  }
  for (const cwd of activeRoots()) if (!left.trees.has(cwd)) loadFiles(cwd);
}

function setProject(cwd) {
  proj.cur = cwd || null;
  pref.set("project", proj.cur ?? "");
  br.cwd = null;
  left.els.clear();
  $("#tree").replaceChildren();
  syncProjects();
  loadBranches();
  render(true);
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
    if (!activeRoots().includes(cwd) || !hits.length) continue;
    left.dirty = true;
    for (const hit of hits) {
      left.flash.set(`${cwd}|${hit.path}`, FLASH[hit.kind]);
      const parts = hit.path.split("/");
      for (let i = 1; i < parts.length; i++) left.lit.add(`${cwd}|${parts.slice(0, i).join("/")}`);
      if (hit.kind === "e" || hit.kind === "w") changedFiles = true;
    }
  }
  if (!changedFiles) return;
  if (left.base === "since") {
    for (const cwd of activeRoots()) {
      const ent = left.trees.get(cwd);
      if (ent?.tree.root != null) setSince(ent);
    }
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
    const id = left.baseSess ?? (isSessionTab(ui.tab) ? ui.tab : projectSessions()[0]?.id);
    return id ? { kind: "session", id } : null;
  }
  if (left.base === "commit") return { kind: "commit", hash: left.hash, cwd: left.hashCwd ?? undefined };
  return { kind: left.base };
}

// 目前看的專案（全部或單一）底下的 session
function projectSessions() {
  const roots = activeRoots();
  return allSessions().filter((s) => s.cwd && roots.includes(projectOf(s.cwd, proj.rootCwds)));
}

// 不帶 cwd：目前看的每個專案都抓一次
async function loadFiles(cwd = null) {
  clearTimeout(left.refetch);
  if (cwd == null) {
    for (const c of activeRoots()) loadFiles(c);
    return;
  }
  const ent = treeFor(cwd);
  const base = currentBase();
  // 樹還沒載入時，commit 基準先用專案的 cwd（另一個 worktree 的根不同，樹要以專案為準）
  const url = base && filesUrl(cwd, ent.tree.root == null && base.kind === "commit" ? { ...base, cwd: undefined } : base);
  if (!url) {
    ent.error = base ? "commit hash 要 4–64 位十六進位" : "這個專案沒有 session";
    render(true);
    return;
  }
  const req = ++ent.req;
  ent.loading = true;
  render(true);
  try {
    const res = await apiFetch(url);
    const body = await res.json().catch(() => ({}));
    if (req !== ent.req) return;
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
    if (cwd === brTarget()) loadBranches();
  } catch (e) {
    if (req !== ent.req) return;
    ent.error = `檔案樹載入失敗：${e.message}`;
  } finally {
    if (req === ent.req) {
      ent.loading = false;
      render(true);
    }
  }
}
setInterval(() => { if (left.tab === "tree" && !document.hidden && left.base !== "since") loadFiles(); }, 60_000);

// ---------- 分支視窗（#696） ----------
// 檔案樹標題列「⎇ 分支（N）」打開，像 VS Code 的 Git Graph：上半是 commit 圖（/api/graph）＋分支標籤（/api/branches），
// 點一列看完整 message 與改到的檔（/api/commit），點檔看 diff（/api/commit-diff）；分支標籤的 ⚠ 看衝突候選（/api/branch-diff 並排）。
// 視窗開著才跟著 file.*／git.op 重抓；關著只在換專案與每 60 秒抓 /api/branches（更新 N）。
const br = { cwd: null, body: null, graph: null, error: null, req: 0, sel: null, detail: null, file: null, detailReq: 0 };
const LANE_COLORS = ["var(--accent)", "var(--teal)", "var(--violet)", "var(--amber)", "var(--rose)", "#3B82F6", "#16A34A", "#DB2777"];
const ROW_PX = 24;
const COL_PX = 14;

async function getJson(url) {
  const res = await apiFetch(url);
  const body = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(body.error ?? `HTTP ${res.status}`);
  return body;
}

// 分支視窗看哪個專案：點過專案標題列的 ⎇ 就是那個；否則單一專案、或唯一的那個專案
const brTarget = () => br.cwd ?? proj.cur ?? (proj.rootCwds.length === 1 ? proj.rootCwds[0] : null);

function openBranches(cwd) {
  if (br.cwd !== cwd) Object.assign(br, { cwd, body: null, graph: null, error: null, sel: null, detail: null, file: null });
  if (brWin.isOpen) loadBranches(cwd);
  else brWin.toggle();
  render(true);
}

async function loadBranches(cwd = brTarget()) {
  if (!cwd || isMonitor) return;
  const req = ++br.req;
  const withGraph = brWin.isOpen;
  try {
    const [body, graph] = await Promise.all([getJson(branchesUrl(cwd)), withGraph ? getJson(graphUrl(cwd)) : null]);
    if (req !== br.req) return;
    if (br.cwd !== cwd) Object.assign(br, { sel: null, detail: null, file: null, graph: null });
    Object.assign(br, { cwd, body, error: null });
    if (graph) br.graph = graph;
  } catch (e) {
    if (req !== br.req) return;
    Object.assign(br, { cwd, body: null, graph: null, error: `分支載入失敗：${e.message}` });
  }
  render(true);
}
setInterval(() => { if (!document.hidden && !brWin.isOpen) loadBranches(); }, 60_000);

const branchOf = (name) => br.body?.branches.find((b) => (b.branch ?? b.worktree) === name);
const branchName = (r) => r.branch ?? worktreeLabel(br.body.root, r.worktree);

// 點一列：commit 或未 commit 的改動 → 完整 message 與改到的檔
async function selectGraphRow(row) {
  const key = row.hash;
  if (br.sel?.key === key) return;
  const req = ++br.detailReq;
  br.sel = { key, kind: row.wip ? "wip" : "commit", row };
  br.detail = { loading: true };
  br.file = null;
  render(true);
  try {
    const d = await getJson(commitUrl(br.cwd, row.wip ? { worktree: row.wip.worktree } : { hash: row.hash }));
    if (req === br.detailReq) br.detail = d;
  } catch (e) {
    if (req === br.detailReq) br.detail = { error: e.message };
  }
  render(true);
}

async function openCommitFile(path) {
  const sel = br.sel;
  if (!sel) return;
  const req = ++br.detailReq;
  const label = sel.kind === "wip" ? `未 commit・${branchName(sel.row.wip)}` : sel.row.hash.slice(0, 7);
  br.file = { path, diffs: [{ branch: label, loading: true }] };
  render(true);
  let d;
  try {
    const body = await getJson(commitDiffUrl(br.cwd, sel.kind === "wip" ? { worktree: sel.row.wip.worktree } : { hash: sel.row.hash }, path));
    d = { branch: label, diff: body.diff, truncated: body.truncated };
  } catch (e) {
    d = { branch: label, error: e.message };
  }
  if (req !== br.detailReq) return;
  br.file = { path, diffs: [d] };
  render(true);
}

// 分支標籤的 ⚠：這條分支和其他分支同時改到的檔
function selectShared(r) {
  ++br.detailReq;
  br.sel = { key: `shared:${r.worktree}`, kind: "shared", row: r };
  br.detail = null;
  br.file = null;
  render(true);
}

// 衝突候選的檔：這條分支和同時改到它的分支，各自對 merge-base 的 diff 並排
async function openBranchDiff(row, file) {
  const rows = [row, ...file.with.map(branchOf)].filter(Boolean);
  const req = ++br.detailReq;
  br.file = { path: file.path, conflict: file.conflict, diffs: rows.map((r) => ({ branch: branchName(r), loading: true })) };
  render(true);
  const got = await Promise.all(rows.map(async (r) => {
    try {
      const body = await getJson(branchDiffUrl(br.cwd, r.worktree, file.path));
      return { branch: branchName(r), diff: body.diff, truncated: body.truncated };
    } catch (e) {
      return { branch: branchName(r), error: e.message };
    }
  }));
  if (req !== br.detailReq) return;
  br.file = { ...br.file, diffs: got };
  render(true);
}

function sessChip(id) {
  return h("button", { class: "bss", title: `${id}\n點一下切到這個 session`, onclick: (e) => { e.stopPropagation(); selectTab(id); } },
    h("i", { style: `background:${sessionColor(id)}` }), h("span", { class: "mono" }, shortId(id)));
}

// 分支標籤：名字、在上面的 session、↑↓、⚠ 衝突候選數
function refBadge(name) {
  const r = branchOf(name);
  const cf = r?.shared.filter((f) => f.conflict).length ?? 0;
  return h("span", { class: `ref${r?.integration ? " int" : ""}`, title: r ? `${name}\n${worktreeLabel(br.body.root, r.worktree)}${r.integration ? "\n整合分支" : `\n相對 ${br.body.integration ?? "—"}：領先 ${r.ahead ?? "—"}、落後 ${r.behind ?? "—"}`}` : name },
    h("b", { class: "mono" }, name),
    ...(r?.sessions ?? []).map(sessChip),
    r && !r.integration && r.ahead != null ? h("small", { class: "mono" }, `↑${r.ahead} ↓${r.behind}`) : null,
    r?.shared.length ? h("button", { class: `shr${cf ? " cf" : ""}`, title: `和其他分支同時改到 ${r.shared.length} 個檔（衝突候選 ${cf}）`, onclick: (e) => { e.stopPropagation(); selectShared(r); } }, `${cf ? "⚠" : "≈"}${r.shared.length}`) : null);
}

function graphSvg(row, width) {
  const w = width * COL_PX + 6;
  const x = (c) => c * COL_PX + COL_PX / 2 + 2;
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("width", w);
  svg.setAttribute("height", ROW_PX);
  svg.setAttribute("class", "gsvg");
  const color = (lane) => LANE_COLORS[lane % LANE_COLORS.length];
  for (const sg of row.segments) {
    const p = document.createElementNS(svg.namespaceURI, "path");
    const [x1, y1, x2, y2] = [x(sg.x1), sg.y1 * ROW_PX, x(sg.x2), sg.y2 * ROW_PX];
    // 斜線用曲線，接近 VS Code 的畫法
    p.setAttribute("d", x1 === x2 ? `M${x1} ${y1}L${x2} ${y2}` : `M${x1} ${y1}C${x1} ${(y1 + y2) / 2} ${x2} ${(y1 + y2) / 2} ${x2} ${y2}`);
    p.setAttribute("stroke", color(sg.lane));
    p.setAttribute("fill", "none");
    p.setAttribute("stroke-width", "2");
    if (row.wip) p.setAttribute("stroke-dasharray", "3 2");
    svg.append(p);
  }
  const dot = document.createElementNS(svg.namespaceURI, "circle");
  dot.setAttribute("cx", x(row.col));
  dot.setAttribute("cy", ROW_PX / 2);
  dot.setAttribute("r", row.wip ? 4 : 4.5);
  dot.setAttribute("stroke", color(row.lane));
  dot.setAttribute("stroke-width", "2");
  dot.setAttribute("fill", row.wip ? "var(--panel)" : color(row.lane));
  svg.append(dot);
  return svg;
}

function when(ts) {
  if (ts == null) return "";
  const d = Date.now() - ts;
  if (d < 86_400_000) return ago(d);
  const t = new Date(ts);
  return `${t.getMonth() + 1}/${t.getDate()}`;
}

function diffCols(diffs) {
  return h("div", { class: "bdc" }, ...diffs.map((d) => h("div", { class: "bdcol" },
    h("div", { class: "bdt mono" }, d.branch.startsWith("未 commit") || /^[0-9a-f]{7}$/.test(d.branch) ? d.branch : `⎇ ${d.branch}`),
    d.loading ? h("div", { class: "none" }, "載入中…")
      : d.error ? h("div", { class: "none" }, `載入失敗：${d.error}`)
        : !d.diff ? h("div", { class: "none" }, "沒有改動")
          : h("div", { class: "bdl" }, ...diffLines(d.diff).map((l) => h("div", { class: `l ${l.kind}` }, l.kind === "add" ? "+" : l.kind === "del" ? "−" : " ", l.text)),
            d.truncated ? h("div", { class: "none" }, "（太長，已截斷）") : null))));
}

function fileList(files, onPick) {
  return h("div", { class: "bfl" }, ...(files.length ? files.map((f) => h("button", {
    class: `bfi${br.file?.path === f.path ? " on" : ""}`, onclick: () => onPick(f),
    title: f.from ? `${f.from} → ${f.path}` : f.path,
  }, h("span", { class: `st s-${f.status === "?" ? "U" : f.status}` }, f.status === "?" ? "U" : f.status), h("span", { class: "mono" }, f.path))) : [h("div", { class: "none" }, "沒有改到檔案")]));
}

function renderDetail() {
  const pane = $("#brDiff");
  pane.hidden = !br.sel;
  if (!br.sel) return;
  const close = h("button", { class: "fw-x", "aria-label": "關閉", onclick: () => { br.sel = null; br.detail = null; br.file = null; render(true); } }, "×");
  const { kind, row } = br.sel;
  let head;
  let files;
  if (kind === "shared") {
    head = h("div", { class: "bdh" }, h("b", { class: "mono" }, `⎇ ${branchName(row)}`), h("small", {}, "和其他分支同時改到的檔・⚠ 是衝突候選（內容不同）；點檔並排看各自對 merge-base 的改動"), close);
    files = h("div", { class: "bfl" }, ...row.shared.map((f) => h("button", {
      class: `bfi${f.conflict ? " cf" : ""}${br.file?.path === f.path ? " on" : ""}`, onclick: () => openBranchDiff(row, f),
    }, f.conflict ? "⚠ " : "", h("span", { class: "mono" }, f.path), h("small", {}, ` ↔ ${f.with.join("、")}`))));
  } else {
    const d = br.detail ?? {};
    const title = kind === "wip" ? `未 commit 的改動・${branchName(row.wip)}` : (d.subject ?? row.subject ?? "");
    head = h("div", { class: "bdh" },
      h("b", {}, title),
      h("small", { class: "mono" }, kind === "wip" ? worktreeLabel(br.body.root, row.wip.worktree) : `${row.hash.slice(0, 10)} · ${d.author ?? row.author ?? ""} · ${when(d.ts ?? row.ts)}`),
      close);
    files = d.loading ? h("div", { class: "none" }, "載入中…")
      : d.error ? h("div", { class: "none" }, `載入失敗：${d.error}`)
        : h("div", {}, d.body ? h("pre", { class: "bmsg" }, d.body) : null, fileList(d.files ?? [], (f) => openCommitFile(f.path)));
  }
  pane.replaceChildren(head, h("div", { class: "bdb" }, files, br.file ? diffCols(br.file.diffs) : null));
}

function renderBranches() {
  const n = changedBranches(br.body);
  const target = brTarget();
  if (changed("brBtn", `${target}|${proj.many}|${br.body ? n : "x"}`)) {
    const btn = $("#brToggle");
    btn.hidden = !target || isMonitor;
    btn.textContent = `${proj.many ? `${proj.labels.get(target) ?? ""} ` : ""}${br.body ? `⎇ 分支（${n}）` : "⎇ 分支"}`;
  }
  if (!brWin.isOpen) return;
  const sig = JSON.stringify([br.error, br.body, br.graph, br.sel?.key, br.detail, br.file, Math.floor(Date.now() / 60_000)]);
  if (!changed("brList", sig)) return;
  const body = br.body;
  $("#brWinSub").textContent = body?.integration ? `相對 ${body.integration}・點一列看檔案・拖標題列移動、右下角縮放` : "拖標題列移動、右下角縮放";
  $("#brWinTitle").textContent = `${proj.rootCwds.length > 1 && br.cwd ? `${proj.labels.get(br.cwd) ?? br.cwd} · ` : ""}${body ? `分支（${body.branches.length}）` : "分支"}`;
  const list = $("#brList");
  if (br.error || !body || !br.graph) {
    list.replaceChildren(h("div", { class: "none" }, br.error ?? "載入中…"));
  } else if (!br.graph.commits.length) {
    list.replaceChildren(h("div", { class: "none" }, "這個專案不是 git repo，或還沒有 commit"));
  } else {
    const g = layoutGraph(withWip(br.graph.commits, body.branches));
    list.replaceChildren(...g.rows.map((r) => h("div", {
      class: `grow${r.wip ? " wip" : ""}${br.sel?.key === r.hash ? " on" : ""}`, role: "button", tabindex: "0",
      onclick: () => selectGraphRow(r),
      onkeydown: (e) => { if (e.key === "Enter") selectGraphRow(r); },
    },
    graphSvg(r, g.width),
    h("span", { class: "gm" },
      ...(r.wip ? [h("span", { class: "ref wipb" }, h("b", { class: "mono" }, branchName(r.wip)), ...r.wip.sessions.map(sessChip))] : (r.refs ?? []).map(refBadge)),
      h("span", { class: "gs" }, r.wip ? `未 commit 的改動（${r.wip.dirty} 個檔）` : r.subject)),
    h("span", { class: "ga" }, r.wip ? "" : r.author ?? ""),
    h("span", { class: "gt" }, r.wip ? "" : when(r.ts)),
    h("span", { class: "gh mono" }, r.wip ? "" : r.hash.slice(0, 7)))),
    ...graphNotices(br.graph).map((t) => h("div", { class: "none" }, t)));
  }
  renderDetail();
}

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
  const roots = activeRoots();
  const loaded = roots.map((c) => left.trees.get(c)).filter(Boolean);
  const one = roots.length === 1 ? loaded[0] : null; // 單一專案、或全部專案但只有一個：資訊列照那棵樹
  const sess = left.base === "session" ? projectSessions() : [];
  const base = currentBase();
  const sig = JSON.stringify([roots, left.base, base?.id, sess.map((s) => s.id), left.hash, loaded.map((e) => [e.info, e.error, e.loading])]);
  if (!changed("tbar", sig)) return;
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
  const errs = loaded.filter((e) => e.error).length;
  info.classList.toggle("err", one ? !!one.error : errs > 0 && errs === loaded.length);
  info.textContent = one
    ? one.error ?? (one.loading && !one.info ? "載入中…" : left.base === "commit" && !left.hash ? "輸入 commit hash，或在單一 session 的 git 列點最後的 commit" : one.info ?? "")
    : !roots.length ? ""
      : left.base === "commit" && !left.hash ? "輸入 commit hash，或在單一 session 的 git 列點最後的 commit"
        : `全部專案（${roots.length} 個）· ${BASE_TEXT[left.base]}${errs ? ` · ${errs} 個載入失敗` : ""}`;
  info.title = info.textContent;
}

function setBase(kind) {
  left.base = kind;
  pref.set("base", kind);
  if (kind !== "commit") left.hashCwd = null;
  if (kind === "since") {
    for (const cwd of activeRoots()) {
      const ent = left.trees.get(cwd);
      if (ent?.tree.root != null) setSince(ent);
      else loadFiles(cwd);
    }
    render(true);
  } else if (kind !== "commit" || /^[0-9a-f]{4,64}$/i.test(left.hash)) loadFiles();
  else render(true);
}
$("#gProj").addEventListener("change", (e) => setProject(e.target.value));
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
  const roots = activeRoots();
  const entries = roots.map((cwd) => ({ cwd, ent: left.trees.get(cwd) })).filter((e) => e.ent && e.ent.tree.root != null);
  if (!entries.length) {
    const one = roots.length === 1 ? left.trees.get(roots[0]) : null;
    treePlaceholder(one?.error ?? (roots.length ? "載入檔案樹…" : "尚無 session"));
    return;
  }
  lastSig.delete("treeNone");
  const box = $("#tree");
  if (left.dirty) {
    // 全部專案：每個專案一段，前面一個標題列（projects.js）；單一專案就是那棵樹
    left.rows = projectTreeRows(entries.map(({ cwd, ent }) => ({ cwd, rows: ent.tree.visible().map((r) => ({ path: r.path, cwd, r, ent })) })), proj.collapsed);
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
    const x = rows[i];
    const key = x.hdr ? `#${x.cwd}` : `${x.cwd}|${x.path}`;
    keep.add(key);
    let sig;
    let make;
    if (x.hdr) {
      const pr = proj.roots.find((p) => p.cwd === x.cwd);
      const ent = left.trees.get(x.cwd);
      sig = `h|${i}|${x.open}|${x.n}|${pr?.live}|${pr?.sessions.length}|${ent?.info}|${ent?.error}|${proj.colors.get(x.cwd)}`;
      make = () => projectHeader(x, pr, ent, i);
    } else {
      const { r, ent } = x;
      const t = ent.tree;
      const c = r.dir ? null : t.counts(r.path);
      const dots = r.dir ? [] : t.touching(r.path, running);
      sig = `${i}|${r.pre}|${r.open}|${r.git}|${r.conflict}|${c ? `${c.r},${c.e},${c.w},${c.x},${c.isNew}` : ""}|${dots.join(",")}`;
      make = () => treeRow(r, c, dots, t, i);
    }
    let row = left.els.get(key);
    if (!row || row.sig !== sig) {
      const el = make();
      if (row) row.el.replaceWith(el);
      else inner.append(el);
      row = { el, sig };
      left.els.set(key, row);
    }
    const fl = left.flash.get(key);
    if (fl || left.lit.has(key)) {
      const el = row.el;
      el.classList.remove("flash", "flash-e", "flash-w", "flash-x", "lit");
      void el.offsetWidth; // 重新觸發動畫
      if (fl) el.classList.add(fl);
      if (left.lit.has(key)) el.classList.add("lit");
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

// 全部專案時每個專案的標題列：專案色、名稱、進行中/全部 session；點一下收起或展開，⎇ 開這個專案的分支視窗
function projectHeader(x, pr, ent, i) {
  const color = projectColor(proj.colors, x.cwd);
  const el = h("div", {
    class: `node phdr${x.open ? " open" : ""}`, style: `top:${i * ROW_H}px`, role: "button", tabindex: "0", "aria-expanded": String(x.open),
    title: `${x.cwd}\n${ent?.error ?? ent?.info ?? ""}`.trim(),
  },
  h("span", { class: "lbl" }, h("span", { class: "pre" }, x.open ? "▾ " : "▸ "), h("i", { class: "psw", style: `background:${color}` }), h("span", { class: "nm", style: `color:${color}` }, proj.labels.get(x.cwd) ?? x.cwd), h("small", {}, ` ${pr?.live ?? 0}/${pr?.sessions.length ?? 0}${ent?.error ? " · 載入失敗" : ""}`)),
  h("span", { class: "mk" }, h("span", { class: "pbr", role: "button", tabindex: "0", title: "這個專案的分支", onclick: (e) => { e.stopPropagation(); openBranches(x.cwd); } }, "⎇")));
  const toggle = () => {
    if (proj.collapsed.has(x.cwd)) proj.collapsed.delete(x.cwd);
    else proj.collapsed.add(x.cwd);
    left.dirty = true;
    render(true);
  };
  el.addEventListener("click", toggle);
  el.addEventListener("keydown", (e) => { if (e.key === "Enter" && e.target === el) toggle(); });
  return el;
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
  if (paneMoreReq || !isSessionTab(ui.tab)) return;
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
  const on = isSessionTab(ui.tab);
  $("#allblk").hidden = ui.tab !== "all";
  $("#spane").hidden = !on;
  $("#replayblk").hidden = ui.tab !== REPLAY_TAB;
  if (ui.tab === REPLAY_TAB) renderReplay();
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
  $("#spReplay").hidden = !canReplay(s);
  $("#spReplayTag").hidden = !pane.replaying;

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

// 單一專案時只列那個專案的（其他的留在 changes 裡，切回來就有）；全部專案且不只一個時依專案分組，組內仍是最新在上
function renderChanges() {
  const list = changes.list.filter((c) => inProject(c.session));
  if (!changed("chg", `${proj.cur}|${proj.many}|${changes.selectedKey}|` + list.map((c) => c.key).join(","))) return;
  const box = $("#chg");
  if (!list.length) {
    box.replaceChildren(h("div", { class: "none" }, changes.list.length ? "這個專案尚無改動" : "尚無改動"));
    return;
  }
  const row = (c) => {
    const n = c.kind === "edit" ? h("span", { class: "n" }, h("span", { class: "a" }, `+${c.added}`), " ", h("span", { class: "d" }, `−${c.removed}`)) : h("span", { class: "n" }, c.isNew ? "新檔案" : "覆寫");
    return h("button", { class: c.key === changes.selectedKey ? "sel" : "", title: `${c.path}\n${c.session}`, "aria-pressed": String(c.key === changes.selectedKey), onclick: () => selectChange(c.key) },
      h("span", { class: "ss", style: `color:${sessionColor(c.session)}` }, shortId(c.session)),
      h("span", { class: `k ${c.kind === "edit" ? "e" : "w"}` }, c.kind === "edit" ? "改" : "寫"),
      h("span", { class: "pth" }, c.path),
      n,
      h("span", { class: "ag" }, agentWho(c.session, c.agent, agentName)));
  };
  const groups = proj.many ? groupByProject(list, (c) => projectOf(cwdOf(c.session), proj.rootCwds)) : [{ cwd: null, items: list }];
  box.replaceChildren(
    ...(changes.following ? [] : [h("button", { class: "hint", onclick: () => selectChange(list[0].key) }, "已停在選中的那筆；點這裡回到最新並恢復自動切換")]),
    ...groups.flatMap((g) => [
      proj.many ? h("div", { class: "pjh", title: g.cwd ?? "不在任何專案底下" }, h("span", { class: "pj", style: g.cwd ? `color:${projectColor(proj.colors, g.cwd)}` : "" }, g.cwd ? proj.labels.get(g.cwd) ?? g.cwd : "其他"), h("small", {}, `${g.items.length} 筆`)) : null,
      ...g.items.map(row),
    ]).filter(Boolean),
  );
}

// diff 以資料存，只在選中時才轉成 HTML
function renderDiff() {
  const sel = changes.selected();
  const c = sel && inProject(sel.session) ? sel : null; // 單一專案時不顯示別的專案的改動
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
// 遠端未載入的思考（#668）：展開那一行時用 API 取全文；hide 模式不會給，直接提示
async function loadThinking(row, v) {
  if (row.type !== "thinking" || !row.deferred || row.loading) return;
  const repaint = () => { row.h = 0; row.v++; v.update({ updated: new Set([row.key]) }); };
  if (ui.welcome?.thinking === "hide") {
    row.loadError = "這個 collector 不對遠端提供思考內容（remote_thinking = hide）";
    repaint();
    return;
  }
  row.loading = true;
  repaint();
  try {
    const r = await apiFetch(`/api/sessions/${encodeURIComponent(row.session)}/thinking/${row.seq}`);
    if (!r.ok) throw new Error(r.status === 403 ? "這個 collector 不對遠端提供思考內容" : `載入失敗（${r.status}）`);
    const j = await r.json();
    row.text = j.text ?? "";
    row.summarized = !!j.summarized;
    row.deferred = false;
    row.loadError = null;
  } catch (e) {
    row.loadError = e?.message ?? String(e);
  }
  row.loading = false;
  repaint();
}

const view = createStreamView({
  prof: benchProf,
  scroller: $("#stream"),
  model: stream,
  agentName,
  projectTag,
  hostTag,
  thinkingMode: () => ui.welcome?.thinking ?? null,
  onExpand: (row) => loadThinking(row, view),
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
  thinkingMode: () => ui.welcome?.thinking ?? null,
  onExpand: (row) => loadThinking(row, paneView),
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

const brWin = makeFloating($("#brWin"), {
  handle: $("#brWinHandle"),
  key: "coding-lab.brWin",
  initial: () => {
    // 全部專案時 ⎇ 在專案標題列、tbar 的按鈕是藏著的（#669）：改靠著 tbar 開
    const b = ($("#brToggle").hidden ? $("#tbar") : $("#brToggle")).getBoundingClientRect();
    return { x: b.left, y: b.bottom + 6, w: 820, h: 560 };
  },
  onToggle: (open) => {
    $("#brToggle").setAttribute("aria-expanded", String(open));
    lastSig.delete("brList");
    if (open) loadBranches();
    render(true);
  },
});
$("#brToggle").addEventListener("click", () => brWin.toggle());
$("#brWinClose").addEventListener("click", () => brWin.close());

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

// ---------- 全專案回放（#702）：所有 session（含已結束）合成一條串流，從 /api/projects/events 由新往舊讀，不接即時更新 ----------
const rp = createReplay();
let rpReq = 0; // 換範圍後，舊的回應丟掉
const rpView = createStreamView({ scroller: $("#rpStream"), model: rp.stream, agentName, thinkingMode: () => ui.welcome?.thinking ?? null, onExpand: (row) => loadThinking(row, rpView), onNeedMore: () => replayMore(), onFollow: () => {} });
const fmtTime = (ms) => new Date(ms).toLocaleString("zh-TW", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit", hour12: false });

function fillReplayProjects() {
  const roots = projectRoots(allSessions());
  const sel = $("#rpProj");
  if (!changed("rpProj", roots.map((r) => r.cwd).join("|"))) return;
  const keep = sel.value || rp.cwd || proj.cur;
  sel.replaceChildren(...roots.map((r) => h("option", { value: r.cwd }, r.cwd)));
  if (roots.some((r) => r.cwd === keep)) sel.value = keep;
}

function startReplay() {
  const cwd = $("#rpProj").value;
  if (!cwd) return;
  const choice = $("#rpRange").value;
  let { from, to } = rangeFor(choice, Date.now(), -new Date().getTimezoneOffset());
  if (choice === "custom") {
    // datetime-local 沒有時區，Date.parse 當成本地時間
    from = Date.parse($("#rpFrom").value);
    to = Date.parse($("#rpTo").value);
    if (!(from <= to)) {
      rp.error = "自訂範圍要填開始與結束，而且開始不能晚於結束";
      render(true);
      return;
    }
  }
  rpReq++;
  rp.reset({ cwd, from, to });
  rpView.reset();
  rpView.setHasMore(false);
  replayMore();
}

async function replayMore() {
  if (rp.loading || rp.done || !rp.cwd) return;
  rp.loading = true;
  const req = rpReq;
  render(true);
  try {
    const res = await apiFetch(replayUrl({ cwd: rp.cwd, from: rp.from, to: rp.to, before: rp.next }));
    const body = await res.json();
    if (req !== rpReq) return;
    if (!res.ok) {
      rp.loading = false;
      rp.error = body?.error ?? `讀取失敗（HTTP ${res.status}）`;
    } else {
      rpView.prepended(rp.load(body));
      rpView.setHasMore(!rp.done);
    }
  } catch {
    if (req !== rpReq) return;
    rp.loading = false;
    rp.error = "讀取失敗（collector 沒有回應，或版本太舊不支援專案回放）";
  }
  render(true);
}

function renderReplay() {
  fillReplayProjects();
  $("#rpCustom").hidden = $("#rpRange").value !== "custom";
  const ids = rp.sessions();
  const n = rp.stream.rows.length;
  const info = rp.error ?? (rp.cwd ? `${fmtTime(rp.from)} – ${fmtTime(rp.to)} · ${ids.length} 個 session · ${n} 行${rp.loading ? " · 載入中…" : rp.done ? (n ? " · 已到範圍開頭" : " · 這段時間沒有動作") : " · 捲到頂按「↑ 載入更多」看更早的"}` : "");
  if (changed("rpInfo", info)) $("#rpInfo").textContent = info;
  if (changed("rpSess", ids.map((id) => `${id}:${rp.stream.hiddenSessions.has(id)}`).join("|"))) {
    $("#rpSess").replaceChildren(
      ...(ids.length
        ? ids.map((id) => {
            const off = rp.stream.hiddenSessions.has(id);
            return h("button", { class: `fchip${off ? " off" : ""}`, title: `${id}\n點一下${off ? "顯示" : "隱藏"}這個 session`, "aria-pressed": String(!off), onclick: () => { rp.stream.setSessionHidden(id, !off); rpView.filterChanged(); render(true); } },
              h("i", { style: `background:${sessionColor(id)}` }), h("span", { class: "mono" }, shortId(id)));
          })
        : [h("span", { class: "none" }, "無")]),
    );
  }
  if (changed("rpKind", [...rp.stream.hiddenKinds].sort().join("|"))) {
    $("#rpKind").replaceChildren(
      ...KIND_LABEL.map(([k, label]) => {
        const off = rp.stream.hiddenKinds.has(k);
        return h("button", { class: `fchip${off ? " off" : ""}`, "aria-pressed": String(!off), onclick: () => { rp.stream.setKindHidden(k, !off); rpView.filterChanged(); render(true); } }, label);
      }),
    );
  }
}
$("#rpGo").addEventListener("click", startReplay);
$("#rpRange").addEventListener("change", () => {
  if ($("#rpRange").value !== "custom") startReplay();
  else render(true);
});
$("#rpProj").addEventListener("change", startReplay);

// ---------- 互動 ----------
// ▶ 回放（#690）：已經開著就清空後從 JSONL 由最新往前讀；否則開 tab，等 session 頻道的 snapshot 進來時改走回放
function replaySession(id) {
  if (ui.tab !== id) {
    ui.replayNext = id;
    selectTab(id);
    return;
  }
  pane.open(id);
  pane.replaying = true;
  paneMoreReq = null;
  paneView.reset();
  paneView.setHasMore(false);
  paneMore(true);
  render(true);
}

function selectTab(id) {
  if (id === ui.tab) return;
  switchTab(client, ui.tab, id);
  ui.tab = id;
  // 第一次打開專案回放：用目前的專案、最近 3 小時直接載入
  if (id === REPLAY_TAB && !rp.cwd) {
    fillReplayProjects();
    startReplay();
  }
  if (isSessionTab(id)) {
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
      if (isSessionTab(ui.tab) && m.ch === `session:${pane.session}`) {
        const r = snapshotLoad(pane.session, m, { replay: ui.replayNext === pane.session });
        ui.replayNext = null;
        pane.load(r.load);
        pane.replaying = r.replay;
        paneMoreReq = null;
        paneView.reset();
        paneHasMore();
        if (r.replay) paneMore(true);
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
      if (isSessionTab(ui.tab) && m.ch === `session:${pane.session}`) paneView.update(pane.apply(m.events));
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
    onStatus: (s) => {
      if (ui.conn !== "incompatible") ui.conn = s;
      // WS 握手被 401 拒絕（cookie 過期、金鑰被撤銷）在瀏覽器只看得到斷線：用一個 API 請求確認，是就導去登入
      if (s === "closed" && !mock) fetch("/api/sessions?limit=1", { credentials: "same-origin" }).then(toLogin).catch(() => {});
      render(true);
    },
    onIncompatible: () => { ui.conn = "incompatible"; render(true); },
    error: onServerError,
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
