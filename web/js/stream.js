// 合流串流的資料模型（不碰 DOM）。規則見 PLAN.md「前端狀態機與合流串流」與 contracts.md §4、§6：
// - 有 pos 的 Event 才進串流，只 append；同一個 (session, pos) 只出現一次。
// - tool.called 沒有 pos 時進暫存區，tool.placed 給 pos 後才進串流；卡片的狀態更新就地改內容、不搬位置。
// - 超過上限從最舊的丟；往上「載入更多」時暫時保留（holding），回到底部再 trim。
// - 篩選只影響可見性，不改順序。

import { editCounts } from "./changes.js";

export const CAP = 3000;
// 結束的 session 留在暫存區的卡片最多留幾張（晚到的 tool.placed 還找得回內容），超過丟最舊的（#757 E-F12）
export const ORPHAN_CAP = 256;

// 串流裡的狀態行：prompt 與 session.status（run 不出現，避免太吵；idle＝使用者按 Esc 中斷，只標示、不通知）
const STATUS_ROWS = new Set(["idle", "wait", "done", "fail", "danger", "ended"]);

const READ_TOOLS = new Set(["Read", "Grep", "Glob", "LS", "NotebookRead", "WebFetch", "WebSearch"]);
const EDIT_TOOLS = new Set(["Edit", "MultiEdit", "NotebookEdit"]);
const WRITE_TOOLS = new Set(["Write", "Task", "Agent"]);

// th 思考、tx 訊息、rd 讀、ed 改、wr 寫／派工、cmd 指令與輸出、st 狀態
export const KINDS = ["th", "tx", "rd", "ed", "wr", "cmd", "st"];

export function kindOf(row) {
  if (row.type === "thinking") return "th";
  if (row.type === "text") return "tx";
  if (row.type === "status") return "st";
  const tool = row.card?.tool;
  if (READ_TOOLS.has(tool)) return "rd";
  if (EDIT_TOOLS.has(tool)) return "ed";
  if (WRITE_TOOLS.has(tool)) return "wr";
  return "cmd";
}

// 跨 session 的順序鍵（contracts.md §6）：(ts_seen, session, pos)；同 session 內只看 server 給的順序（依 pos）
function compareRows(a, b) {
  return a.ts - b.ts || (a.session < b.session ? -1 : a.session > b.session ? 1 : 0) || (a.pos ?? -1) - (b.pos ?? -1) || a.seq - b.seq;
}


export function createStream({ cap = CAP } = {}) {
  const st = {
    rows: [],
    staged: new Map(), // toolUseId → card
    holding: false,
    cap,
    hiddenSessions: new Set(),
    hiddenKinds: new Set(),
    sessionFilter: null, // 單一專案（#669）：session id → 是否顯示；null＝全部
  };
  const keys = new Set(); // 已在串流的 row key
  const cards = new Map(); // toolUseId → card（暫存區與串流共用同一個物件）
  const cardRow = new Map(); // toolUseId → row
  const dropped = new Set(); // 已被上限丟掉的卡片：之後的更新不讓它復活
  const orphans = new Map(); // 結束的 session 留在暫存區的卡片（toolUseId → card），最多 ORPHAN_CAP 張

  function card(id, ev) {
    let c = cards.get(id);
    if (!c && orphans.has(id)) {
      c = orphans.get(id);
      orphans.delete(id);
      cards.set(id, c);
    }
    if (!c) {
      c = { toolUseId: id, session: ev.session, agent: ev.agent, tool: null, input: {}, status: "pending", danger: null, fallback: false };
      cards.set(id, c);
    }
    return c;
  }

  function touch(id, ops) {
    const r = cardRow.get(id);
    if (r) {
      r.v++;
      // 展開中的行內容變了（例如結果到了）：高度要重量，否則下面的行會疊上來（#747 F-1）
      if (r.expanded) r.h = 0;
      ops.updated.add(r.key);
    } else if (st.staged.has(id)) {
      ops.stagedChanged = true;
    }
  }

  function newRow(ev, fields, out) {
    const key = fields.key ?? `${ev.session}:${ev.pos}`;
    if (keys.has(key)) return null;
    const r = { key, session: ev.session, agent: ev.agent, pos: ev.pos, ts: ev.ts_seen, seq: ev.seq, v: 0, expanded: false, h: 0, delayed: false, ...fields };
    keys.add(key);
    out.push(r);
    return r;
  }

  function placeCard(ev, c, out) {
    if (cardRow.has(c.toolUseId)) return;
    if (st.staged.delete(c.toolUseId)) out.stagedChanged = true;
    const r = newRow(ev, { type: "tool", card: c }, out);
    if (r) cardRow.set(c.toolUseId, r);
  }

  // out 是要接上新行的陣列（append 時接在 rows 後面，prepend 時另外排序），同時記錄 ops
  function handle(ev, out, ops) {
    const p = ev.payload ?? {};
    const id = p.toolUseId;
    if (id != null && dropped.has(id)) return;
    switch (ev.type) {
      case "tool.called": {
        const c = card(id, ev);
        // danger 也可能由先套用的 tool.updated 帶來（transcript 先到時），沒有就不蓋掉
        Object.assign(c, { tool: p.tool, input: p.input ?? {}, danger: p.danger ?? c.danger, agent: ev.agent });
        // tool.updated 可能先被套用（例如載入更多的批次），已有更新的狀態就不蓋回去
        if (c.status === "pending") c.status = p.status ?? "pending";
        if (ev.pos != null) placeCard(ev, c, out);
        else if (!cardRow.has(id)) {
          st.staged.set(id, c);
          ops.stagedChanged = true;
        }
        touch(id, ops);
        break;
      }
      case "tool.placed": {
        const c = card(id, ev);
        c.fallback = p.placement === "fallback";
        placeCard(ev, c, out);
        break;
      }
      case "tool.updated": {
        const c = card(id, ev);
        Object.assign(c, { status: p.status ?? c.status, durationMs: p.durationMs, error: p.error, interrupted: p.interrupted });
        // transcript 先建卡、PreToolUse 後到才判斷出的危險（contracts.md §4）；之後沒帶 danger 的更新不清掉
        if (p.danger != null) c.danger = p.danger;
        touch(id, ops);
        break;
      }
      case "tool.result":
        card(id, ev).result = { ok: p.ok, exitCode: p.exitCode, output: p.output ?? "", truncated: !!p.truncated, redacted: !!p.redacted };
        touch(id, ops);
        break;
      case "file.edit":
        card(id, ev).edit = { path: p.path, ...editCounts(p) };
        touch(id, ops);
        break;
      case "file.write":
        // lines 是全文行數（content 只留前 200 行，#682）；舊 Event 沒有時數預覽
        card(id, ev).write = { path: p.path, isNew: !!p.isNew, lines: p.lines ?? (p.content ?? "").replace(/\n$/, "").split("\n").length };
        touch(id, ops);
        break;
      case "file.read":
        card(id, ev).read = { path: p.path };
        touch(id, ops);
        break;
      case "git.op":
        card(id, ev).git = { op: p.op, branch: p.branch, hash: p.hash, message: p.message, ok: p.ok };
        touch(id, ops);
        break;
      case "agent.spawned":
        if (p.parentToolUseId != null && !dropped.has(p.parentToolUseId)) {
          card(p.parentToolUseId, ev).agentName = p.name;
          touch(p.parentToolUseId, ops);
        }
        break;
      case "assistant.thinking":
      case "assistant.text":
        if (ev.pos != null) {
          newRow(ev, { type: ev.type === "assistant.thinking" ? "thinking" : "text", text: p.text ?? "", summarized: !!p.summarized, delayed: p.delayed === true, deferred: p.deferred === true, chars: p.chars ?? null }, out);
        }
        break;
      case "prompt.submitted":
        // source: "task" 是背景工作完成叫醒 Claude，不是使用者輸入（contracts.md §4）
        newRow(ev, { key: `${ev.session}:s${ev.seq}`, type: "status", status: "prompt", source: p.source === "task" ? "task" : "user", text: p.text ?? "" }, out);
        break;
      case "session.status":
        if (STATUS_ROWS.has(p.status)) newRow(ev, { key: `${ev.session}:s${ev.seq}`, type: "status", status: p.status, text: "" }, out);
        break;
      case "agent.finished": {
        // subagent 結束（#641）：一行狀態。status 省略或 completed＝完成、killed＝被停掉（TaskStop），其他＝失敗（#677）
        const result = p.status == null || p.status === "completed" ? "done" : p.status === "killed" ? "killed" : "fail";
        newRow(ev, { key: `${ev.session}:s${ev.seq}`, type: "status", status: "agent", agentId: p.agentId, result, text: p.lastMessage ?? "" }, out);
        break;
      }
      case "session.started":
        // resume／compact 接回同一個 session（#651）：一行狀態。startup 與 clear 都是新 session 的第一行，不必標
        // （`/clear` 換 session id，#688 實測）；判斷和 collector 的 is_status_line 一致
        if (p.source === "resume" || p.source === "compact") newRow(ev, { key: `${ev.session}:s${ev.seq}`, type: "status", status: "session", source: p.source, text: "" }, out);
        break;
      case "session.ended":
        // 中途結束的 session：還在暫存區的卡片不會再有進展，從「即時」計數拿掉。
        // 卡片移到 orphans（有上限，不然結束的 session 越多 cards 越大），萬一 tool.placed 晚到仍照常進串流。
        for (const [tid, c] of st.staged) {
          if (c.session === ev.session) {
            st.staged.delete(tid);
            cards.delete(tid);
            orphans.set(tid, c);
            ops.stagedChanged = true;
          }
        }
        for (const tid of orphans.keys()) {
          if (orphans.size <= ORPHAN_CAP) break;
          orphans.delete(tid);
        }
        break;
      default:
        break;
    }
  }

  function dropFront(n) {
    const gone = st.rows.splice(0, n);
    for (const r of gone) {
      keys.delete(r.key);
      if (r.card) {
        cardRow.delete(r.card.toolUseId);
        cards.delete(r.card.toolUseId);
        dropped.add(r.card.toolUseId);
      }
    }
    // dropped 只需要記得最近的；避免無限成長
    if (dropped.size > st.cap * 4) {
      const keep = [...dropped].slice(-st.cap * 2);
      dropped.clear();
      for (const id of keep) dropped.add(id);
    }
    return gone;
  }

  function newOps() {
    return { appended: 0, updated: new Set(), dropped: 0, droppedRows: [], stagedChanged: false };
  }

  st.apply = (events) => {
    const ops = newOps();
    const out = [];
    for (const ev of events ?? []) handle(ev, out, ops);
    if (out.stagedChanged) ops.stagedChanged = true;
    st.rows.push(...out);
    ops.appended = out.length;
    // 停在上方（holding，看載入更多的內容）時放寬到 3 倍，但不能沒有上限：開著不動也會一直長（#766 F-4）
    const limit = st.holding ? st.cap * 3 : st.cap;
    if (st.rows.length > limit) {
      ops.droppedRows = dropFront(st.rows.length - limit);
      ops.dropped = ops.droppedRows.length;
    }
    return ops;
  };

  st.load = ({ staged = [], stream = [] } = {}) => {
    st.rows = [];
    st.staged.clear();
    st.holding = false;
    keys.clear();
    cards.clear();
    cardRow.clear();
    dropped.clear();
    orphans.clear();
    return st.apply([...staged, ...stream]);
  };

  // 載入更多：每個 session 一段 Event（server 已依 pos 排好、狀態行插在該在的位置），
  // 各 session 依開頭那行的順序鍵逐行合併，不打亂同 session 內的順序（contracts.md §6），放在最上面
  st.prepend = (batches) => {
    const ops = newOps();
    const made = [];
    for (const events of batches ?? []) for (const ev of events ?? []) handle(ev, made, ops);
    const bySession = new Map();
    for (const r of made) {
      if (!bySession.has(r.session)) bySession.set(r.session, []);
      bySession.get(r.session).push(r);
    }
    const queues = [...bySession.values()];
    const heads = queues.map(() => 0);
    const out = [];
    for (;;) {
      let best = -1;
      for (let i = 0; i < queues.length; i++) {
        if (heads[i] < queues[i].length && (best < 0 || compareRows(queues[i][heads[i]], queues[best][heads[best]]) < 0)) best = i;
      }
      if (best < 0) break;
      out.push(queues[best][heads[best]++]);
    }
    st.rows = out.concat(st.rows);
    if (out.length) st.holding = true;
    return { prepended: out.length, rows: out, updated: ops.updated };
  };

  st.trim = () => {
    st.holding = false;
    const n = st.rows.length - st.cap;
    const gone = n > 0 ? dropFront(n) : [];
    return { dropped: gone.length, droppedRows: gone };
  };

  // 每個 session 在串流裡最小的 pos（「載入更多」的 beforePos）
  st.minPos = () => {
    const m = new Map();
    for (const r of st.rows) if (r.pos != null && (!m.has(r.session) || r.pos < m.get(r.session))) m.set(r.session, r.pos);
    return m;
  };

  st.setSessionHidden = (id, hidden) => (hidden ? st.hiddenSessions.add(id) : st.hiddenSessions.delete(id));
  st.setKindHidden = (k, hidden) => (hidden ? st.hiddenKinds.add(k) : st.hiddenKinds.delete(k));
  st.setSessionFilter = (fn) => { st.sessionFilter = typeof fn === "function" ? fn : null; };
  // 記憶體裡的卡片張數（測試用）
  st.cardCount = () => cards.size;
  st.isVisible = (r) => !st.hiddenSessions.has(r.session) && !st.hiddenKinds.has(kindOf(r)) && (st.sessionFilter == null || st.sessionFilter(r.session));

  return st;
}
