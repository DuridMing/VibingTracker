// 彙總列（contracts.md §6 的 rows）。snapshot 整批取代，frame 以 id upsert。
// 串流事件的排序與位置不在這裡（#639）。

export function createStore() {
  return { sessions: new Map(), kpi: {} };
}

function upsert(map, rows) {
  if (!Array.isArray(rows)) return;
  for (const row of rows) {
    if (row == null || row.id == null) continue;
    map.set(row.id, { ...map.get(row.id), ...row });
  }
}

export function applySnapshot(st, msg) {
  st.sessions = new Map();
  upsert(st.sessions, msg.sessions);
  st.kpi = { ...msg.kpi };
}

export function applyFrame(st, msg) {
  const rows = msg.rows;
  if (rows == null) return;
  upsert(st.sessions, rows.sessions);
  if (rows.kpi != null) st.kpi = { ...st.kpi, ...rows.kpi };
}
