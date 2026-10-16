// 一個 session 的 agents 整理成畫面要的樣子（contracts.md §6 `agents`）。
// Agents 面板現在用，P4 單 session 畫面的 lanes 也用同一份，狀態只有 store 裡的 rows。

const oneLine = (s) => (typeof s === "string" ? s.replace(/\s+/g, " ").trim() : "");

export function agentLabel(a) {
  if (a.agentId === "main") return "main";
  if (a.agentId === "bg") return "背景";
  return oneLine(a.name) || oneLine(a.agentType) || String(a.agentId).slice(0, 8);
}

export const LINGER_MS = 5000;

// rows：store.agents 的值；sessionModel：sessions 列的 model（main 的模型）
// now：給了就把 LINGER_MS 內剛完成的 subagent 放進 recent（Agents 面板先留在標籤列，#641），之後才收進 done
export function agentView(rows, session, { sessionModel = null, now = null, lingerMs = LINGER_MS } = {}) {
  let main = null;
  let bg = null;
  const active = [];
  const done = [];
  for (const a of rows) {
    if (a.session !== session) continue;
    const v = { ...a, label: agentLabel(a), model: null };
    if (a.agentId === "main") {
      v.model = sessionModel ?? null;
      main = v;
    } else if (a.agentId === "bg") {
      bg = v;
    } else {
      // subagent 可以用不同模型；跟 main 一樣就不重複標
      v.model = a.model && a.model !== sessionModel ? a.model : null;
      (a.status === "done" ? done : active).push(v);
    }
  }
  active.sort((x, y) => (x.spawnedAt ?? 0) - (y.spawnedAt ?? 0) || String(x.agentId).localeCompare(String(y.agentId)));
  done.sort((x, y) => (y.finishedAt ?? 0) - (x.finishedAt ?? 0));
  const recent = now == null ? [] : done.filter((a) => a.finishedAt != null && now - a.finishedAt <= lingerMs);
  const settled = recent.length ? done.filter((a) => !recent.includes(a)) : done;
  return { main, bg, active, recent, done: settled, running: active.filter((a) => a.status === "run").length };
}
