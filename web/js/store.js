// 彙總列（contracts.md §6「彙總列」）。snapshot 整批取代，frame 以 id upsert，`deleted: true` 移除。
// 串流事件的排序與位置不在這裡（#639）。

const KINDS = ["sessions", "agents", "attention"];

export function createStore() {
  return { sessions: new Map(), agents: new Map(), attention: new Map(), kpi: {} };
}

function upsert(map, rows) {
  if (!Array.isArray(rows)) return;
  for (const row of rows) {
    if (row == null || row.id == null) continue;
    if (row.deleted === true) map.delete(row.id);
    else map.set(row.id, { ...map.get(row.id), ...row });
  }
}

export function applySnapshot(st, msg) {
  for (const k of KINDS) {
    st[k] = new Map();
    upsert(st[k], msg[k]);
  }
  st.kpi = { ...msg.kpi };
}

export function applyFrame(st, msg) {
  const rows = msg.rows;
  if (rows == null) return;
  for (const k of KINDS) upsert(st[k], rows[k]);
  if (rows.kpi != null) st.kpi = { ...st.kpi, ...rows.kpi };
}
