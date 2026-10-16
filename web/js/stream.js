// 合流串流的資料模型（不碰 DOM）。規則見 PLAN.md「前端狀態機與合流串流」與 contracts.md §4、§6：
// - 有 pos 的 Event 才進串流，只 append；同一個 (session, pos) 只出現一次。
// - tool.called 沒有 pos 時進暫存區，tool.placed 給 pos 後才進串流；卡片的狀態更新就地改內容、不搬位置。
// - 超過上限從最舊的丟；往上「載入更多」時暫時保留（holding），回到底部再 trim。
// - 篩選只影響可見性，不改順序。

export const CAP = 3000;

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

// 跨 session 的順序鍵（contracts.md §6）：(ts_seen, session, pos)
function compareRows(a, b) {
  return a.ts - b.ts || (a.session < b.session ? -1 : a.session > b.session ? 1 : 0) || (a.pos ?? -1) - (b.pos ?? -1) || a.seq - b.seq;
}

function countHunk(lines) {
  let added = 0;
  let removed = 0;
  for (const l of lines ?? []) {
    if (l.startsWith("+")) added++;
    else if (l.startsWith("-")) removed++;
  }
  return { added, removed };
}

export function createStream({ cap = CAP } = {}) {
  const st = {
    rows: [],
    staged: new Map(), // toolUseId → card
    holding: false,
    cap,
    hiddenSessions: new Set(),
    hiddenKinds: new Set(),
  };
  const keys = new Set(); // 已在串流的 row key
  const cards = new Map(); // toolUseId → card（暫存區與串流共用同一個物件）
  const cardRow = new Map(); // toolUseId → row
  const dropped = new Set(); // 已被上限丟掉的卡片：之後的更新不讓它復活

  function card(id, ev) {
    let c = cards.get(id);
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
        Object.assign(c, { tool: p.tool, input: p.input ?? {}, danger: p.danger ?? null, agent: ev.agent });
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
        touch(id, ops);
        break;
      }
      case "tool.result":
        card(id, ev).result = { ok: p.ok, exitCode: p.exitCode, output: p.output ?? "", truncated: !!p.truncated };
        touch(id, ops);
        break;
      case "file.edit":
        card(id, ev).edit = { path: p.path, ...countHunk(p.hunk?.lines) };
        touch(id, ops);
        break;
      case "file.write":
        card(id, ev).write = { path: p.path, isNew: !!p.isNew, lines: (p.content ?? "").replace(/\n$/, "").split("\n").length };
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
          newRow(ev, { type: ev.type === "assistant.thinking" ? "thinking" : "text", text: p.text ?? "", summarized: !!p.summarized, delayed: p.delayed === true, deferred: p.deferred === true }, out);
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
        // 卡片本身留著，萬一 tool.placed 晚到仍照常進串流。
        for (const [tid, c] of st.staged) {
          if (c.session === ev.session) {
            st.staged.delete(tid);
            ops.stagedChanged = true;
          }
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
    if (!st.holding && st.rows.length > st.cap) {
      ops.droppedRows = dropFront(st.rows.length - st.cap);
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
    return st.apply([...staged, ...stream]);
  };

  // 載入更多：每個 session 一段 Event，合併排序後放在最上面
  st.prepend = (batches) => {
    const ops = newOps();
    const out = [];
    for (const events of batches ?? []) for (const ev of events ?? []) handle(ev, out, ops);
    out.sort(compareRows);
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
  st.isVisible = (r) => !st.hiddenSessions.has(r.session) && !st.hiddenKinds.has(kindOf(r));

  return st;
}
