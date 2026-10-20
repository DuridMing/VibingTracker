// 彙總列（contracts.md §6「彙總列」）。snapshot 整批取代，frame 以 id upsert，`deleted: true` 移除。
// 串流事件的排序與位置不在這裡（#639）。

const KINDS = ["sessions", "agents", "attention"];

export function createStore() {
  return { sessions: new Map(), agents: new Map(), attention: new Map(), kpi: {}, storeError: null };
}

function upsert(map, rows) {
  if (!Array.isArray(rows)) return;
  for (const row of rows) {
    if (row == null || row.id == null) continue;
    if (row.deleted === true) map.delete(row.id);
    // hub 標離線的列只會帶 id／host／offline（#663）：沒看過的 id 不能因此變成一列空的 session（#723）
    else if (row.offline === true && !map.has(row.id)) continue;
    else map.set(row.id, { ...map.get(row.id), ...row });
  }
}

export function applySnapshot(st, msg) {
  for (const k of KINDS) {
    st[k] = new Map();
    upsert(st[k], msg[k]);
  }
  st.kpi = { ...msg.kpi };
  // 資料寫不進磁碟（#781 W-M10）：snapshot 一定帶，舊的 collector 沒有這個欄位就當正常
  st.storeError = msg.storeError ?? null;
}

export function applyFrame(st, msg) {
  // frame 只在 storeError 改變時帶這個鍵（null＝恢復），沒帶就維持
  if ("storeError" in msg) st.storeError = msg.storeError ?? null;
  const rows = msg.rows;
  if (rows == null) return;
  for (const k of KINDS) upsert(st[k], rows[k]);
  if (rows.kpi != null) st.kpi = { ...st.kpi, ...rows.kpi };
}
