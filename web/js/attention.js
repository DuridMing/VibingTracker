// 注意佇列的排序（contracts.md §6 `attention`、PLAN「注意佇列」）：
// 種類優先（危險 > 失敗 > 等你 > 完成待確認 > 衝突），同種類依該 session 的 `sessions.score` 由高到低，再依 raisedAt 由舊到新。

export const KIND_ORDER = { danger: 0, fail: 1, wait: 2, done: 3, conflict: 4 };
export const KIND_TEXT = { danger: "危險", fail: "失敗", wait: "等你", done: "完成", conflict: "衝突" };

const rank = (k) => KIND_ORDER[k] ?? 99;

// scoreOf(sessionId) → number | undefined
export function sortAttention(rows, scoreOf) {
  const score = (r) => Number(scoreOf(r.session)) || 0;
  return [...rows].sort((a, b) =>
    rank(a.kind) - rank(b.kind) || score(b) - score(a) || (a.raisedAt ?? 0) - (b.raisedAt ?? 0) || String(a.id).localeCompare(String(b.id)));
}
