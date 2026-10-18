// 注意佇列的排序（contracts.md §6 `attention`、PLAN「注意佇列」）：
// 種類優先（危險 > 失敗 > 等你 > 完成待確認 > 衝突），同種類依 raisedAt 由新到舊（最新的在最上面，#728；不看 session 分數）。
// 危險列在工具結束後留到確認（`clearedAt` 有值）：仍算危險類，但排在還沒結束的危險之後。

export const KIND_ORDER = { danger: 0, fail: 1, wait: 2, done: 3, conflict: 4 };
export const KIND_TEXT = { danger: "危險", fail: "失敗", wait: "等你", done: "完成", conflict: "衝突" };

const rank = (k) => KIND_ORDER[k] ?? 99;
const cleared = (r) => (r.clearedAt != null ? 1 : 0);

export function sortAttention(rows) {
  return [...rows].sort((a, b) =>
    rank(a.kind) - rank(b.kind) || cleared(a) - cleared(b) || (b.raisedAt ?? 0) - (a.raisedAt ?? 0) || String(a.id).localeCompare(String(b.id)));
}

// 單 session 畫面的危險提示框：這個 session 的危險列（未結束的在前）
export function sessionDangers(rows, session) {
  return sortAttention(rows.filter((r) => r.session === session && r.kind === "danger"));
}

// 「全部確認」（#697）：要送 ack 的列，照佇列順序；已在確認中（pending 有它的 id）的不重送
export function ackAllTargets(rows, pending) {
  return sortAttention(rows).filter((r) => !pending.has(r.id));
}
