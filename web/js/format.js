// 串流每一行的顯示（不碰 DOM，回傳 HTML 字串）。所有來自 Event 的文字都經過 esc。
// 每行固定一行高：html 只放一行，可展開的全文放 detail（純文字，由 view 跳脫後顯示）。

import { sessionColor, shortId } from "./color.js";
import { kindOf } from "./stream.js";

export function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

const firstLine = (s) => String(s ?? "").split("\n", 1)[0];
const LONG = 160; // 超過這個長度就可以展開看全文

function agentLabel(row, agentName) {
  if (row.agent === "main") return "main";
  if (row.agent === "bg") return "背景";
  return agentName?.(row.session, row.agent) ?? shortId(row.agent);
}

function duration(ms) {
  if (ms == null) return "";
  return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
}

function statusSuffix(c) {
  if (c.interrupted) return ' <span class="bad">中斷</span>';
  switch (c.status) {
    case "pending": return ' <span class="o">排隊中</span>';
    case "running": return ' <span class="run">…</span>';
    case "done": return ` <span class="ok">✓ ${duration(c.durationMs)}</span>`.replace(" </span>", "</span>");
    case "failed": {
      const why = c.result?.exitCode != null ? `exit ${c.result.exitCode}` : c.error ?? "失敗";
      return ` <span class="bad">✗ ${esc(why)}</span>`;
    }
    default: return "";
  }
}

function toolBody(c) {
  // 載入更多或 snapshot 時伴隨的 tool.called 缺了（contracts.md §6），只知道 toolUseId
  if (c.tool == null) return '<span class="o">（內容不可用）</span>';
  const i = c.input ?? {};
  const path = c.edit?.path ?? c.write?.path ?? c.read?.path ?? i.file_path ?? i.notebook_path ?? i.path ?? "";
  switch (c.tool) {
    case "Bash": {
      const git = c.git ? ` <span class="ok">⎇ ${esc(c.git.op)}${c.git.hash ? ` ${esc(c.git.hash)}` : ""}</span>` : "";
      return `<span class="p">$</span> <span class="c">${esc(firstLine(i.command))}</span>${git}`;
    }
    case "Read":
    case "NotebookRead":
      return `<span class="rd">read</span>  ${esc(path)}`;
    case "Grep":
    case "Glob":
      return `<span class="rd">${c.tool.toLowerCase()}</span>  ${esc(i.pattern ?? "")}${i.path ? `  <span class="o">${esc(i.path)}</span>` : ""}`;
    case "Edit":
    case "MultiEdit":
    case "NotebookEdit": {
      const n = c.edit ? `  <span class="ok">+${c.edit.added}</span> <span class="bad">−${c.edit.removed}</span>` : "";
      return `<span class="ed">edit</span>  ${esc(path)}${n}`;
    }
    case "Write": {
      const n = c.write ? `  <span class="o">（${c.write.isNew ? "新檔案 · " : ""}${c.write.lines} 行）</span>` : "";
      return `<span class="wr">write</span> ${esc(path)}${n}`;
    }
    case "Task":
    case "Agent":
      return `<span class="wr">task</span>  → 派出 <b>${esc(c.agentName ?? i.description ?? i.subagent_type ?? "subagent")}</b>`;
    default: {
      const arg = i.url ?? i.query ?? i.command ?? path ?? "";
      return `<span class="o">${esc(c.tool ?? "tool")}</span>  ${esc(firstLine(typeof arg === "string" ? arg : JSON.stringify(arg)))}`;
    }
  }
}

function toolDetail(c) {
  const cmd = c.tool === "Bash" ? String(c.input?.command ?? "") : "";
  const out = c.result?.output ?? "";
  const parts = [];
  if (cmd && (cmd.includes("\n") || cmd.length > LONG || out || c.error)) parts.push(`$ ${cmd}`);
  if (out) parts.push(out + (c.result.truncated ? "\n…（已截斷，全文在 transcript）" : ""));
  if (c.error && !out) parts.push(c.error);
  return parts.length ? parts.join("\n") : null;
}

const STATUS_TEXT = {
  wait: '<span class="st wait">⏸ 等你回答</span>',
  done: '<span class="st done">✔ 完成，等你確認</span>',
  fail: '<span class="st bad">✗ 失敗</span>',
  danger: '<span class="st bad">⚠ 危險指令</span>',
  ended: '<span class="st">■ session 結束</span>',
};

export function formatRow(row, { agentName } = {}) {
  const kind = kindOf(row);
  const cls = ["ln", kind];
  const tags = [];
  let body = "";
  let detail = null;

  if (row.type === "thinking") {
    body = row.deferred ? '<span class="lbl">think</span> <span class="o">（思考內容未載入）</span>' : `<span class="lbl">think</span> ${esc(firstLine(row.text))}`;
    if (row.summarized) tags.push("摘要");
    detail = row.text || null;
  } else if (row.type === "text") {
    body = `<span class="tx">${esc(firstLine(row.text))}</span>`;
    detail = row.text.includes("\n") || row.text.length > LONG ? row.text : null;
  } else if (row.type === "status") {
    body = row.status === "prompt" ? `<span class="p">▶</span> <span class="pr">${esc(firstLine(row.text))}</span>` : STATUS_TEXT[row.status] ?? esc(row.status);
    detail = row.status === "prompt" && (row.text.includes("\n") || row.text.length > LONG) ? row.text : null;
  } else {
    const c = row.card;
    body = toolBody(c) + statusSuffix(c);
    if (c.danger) {
      cls.push("dz");
      body += ` <span class="warn">⚠ ${esc(c.danger.why ?? c.danger.rule)}</span>`;
    }
    if (c.fallback) tags.push("無 transcript");
    if (c.status === "pending") cls.push("pending");
    detail = toolDetail(c);
  }
  if (row.delayed) tags.push("延遲");
  if (detail != null) cls.push("can-open");
  if (row.expanded) cls.push("open");

  const prefix = `<span class="ss" style="color:${sessionColor(row.session)}" title="${esc(row.session)}">[${esc(shortId(row.session))}]</span><span class="tg">[${esc(agentLabel(row, agentName))}]</span>`;
  const tagHtml = tags.map((t) => ` <span class="tag">${esc(t)}</span>`).join("");
  return { cls: cls.join(" "), html: `${prefix}${body}${tagHtml}`, detail };
}
