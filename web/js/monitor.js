// 監視器視窗（#650）：同一頁加 `#monitor=<session id>`，自己連 collector、只訂 `session:<id>`，不依賴主視窗。

const UUID = /^#monitor=([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$/i;

// 回傳 session id（小寫），不是監視器就回 null；只收完整的 UUID，其他內容一律不當成 id
export function parseMonitor(hash) {
  const m = UUID.exec(hash ?? "");
  return m ? m[1].toLowerCase() : null;
}

// 監視器只訂自己的 session；主視窗訂 `*`（session 頻道在切 tab 時才訂）
export function channelsFor(monitorId) {
  return monitorId ? [`session:${monitorId}`] : ["*"];
}

// 保留 query（例如 ?mock=12），只換 hash
export function monitorUrl(loc, id) {
  return `${loc.pathname}${loc.search}#monitor=${id}`;
}
