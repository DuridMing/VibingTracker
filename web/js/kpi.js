// 頂部 KPI 的數字格式與依模型分列（contracts.md §6 `kpi`、`kpi.models`）。
// 數字都由 collector 算好，這裡只排版，不加總。

export function fmtKpi(k, v) {
  if (v == null) return "—";
  if (k === "costUsd") return `$${v.toFixed(2)}`;
  if (k === "thinkingTokens") return v >= 1000 ? `${(v / 1000).toFixed(1)}k` : String(v);
  return String(v);
}

// models：{ 短名: { costUsd, thinkingTokens, ids } }；回傳依 key 由大到小、附占比的列。
// title 是滑過提示：這個短名底下的完整模型 id（#643），collector 沒給就用短名
export function modelBreakdown(models, key) {
  if (models == null) return [];
  const rows = Object.entries(models).map(([model, m]) => ({ model, value: m?.[key] ?? 0, title: m?.ids?.length ? m.ids.join("\n") : model }));
  const total = rows.reduce((a, r) => a + r.value, 0);
  for (const r of rows) r.share = total > 0 ? r.value / total : 0;
  return rows.sort((a, b) => b.value - a.value || a.model.localeCompare(b.model));
}
