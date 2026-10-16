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

// 工具輸出只顯示前幾行（PLAN「風險」：tool_response 很大），collector 存的是前 8 KB
const OUTPUT_LINES = 20;

function outputText(r) {
  const lines = r.output.split("\n");
  if (lines.length > OUTPUT_LINES) {
    return `${lines.slice(0, OUTPUT_LINES).join("\n")}\n…（共 ${lines.length}${r.truncated ? "+" : ""} 行，只顯示前 ${OUTPUT_LINES} 行）`;
  }
  return r.output + (r.truncated ? "\n…（已截斷，全文在 transcript）" : "");
}

function toolDetail(c) {
  const cmd = c.tool === "Bash" ? String(c.input?.command ?? "") : "";
  const out = c.result?.output ?? "";
  const parts = [];
  if (cmd && (cmd.includes("\n") || cmd.length > LONG || out || c.error)) parts.push(`$ ${cmd}`);
  if (c.result?.redacted) parts.push("（敏感輸出，例如 .env：遠端連線不顯示，請在本機看）");
  else if (out) parts.push(outputText(c.result));
  if (c.error && !out) parts.push(c.error);
  return parts.length ? parts.join("\n") : null;
}

const STATUS_TEXT = {
  idle: '<span class="st idle">⏹ 已中斷</span>',
  wait: '<span class="st wait">⏸ 等你回答</span>',
  done: '<span class="st done">✔ 完成，等你確認</span>',
  fail: '<span class="st bad">✗ 失敗</span>',
  danger: '<span class="st bad">⚠ 危險指令</span>',
  ended: '<span class="st">■ session 結束</span>',
};

// session.started 的來源（#651）：同一個 session 接回來，串流裡標一行
const SESSION_TEXT = {
  resume: '<span class="st">↻ 已 resume，接續先前的對話</span>',
  compact: '<span class="st">⇣ 已 compact，之前的對話已摘要</span>',
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
    if (row.status === "prompt" && row.source === "task") {
      // 背景工作完成叫醒 Claude：不是使用者的 prompt，內容放進展開
      body = '<span class="st task">⟲ 背景工作完成</span>';
      detail = row.text || null;
    } else if (row.status === "prompt") {
      body = `<span class="p">▶</span> <span class="pr">${esc(firstLine(row.text))}</span>`;
      detail = row.text.includes("\n") || row.text.length > LONG ? row.text : null;
    } else if (row.status === "agent") {
      // subagent 結束：名稱與 Agents 面板同一套（agentName），最後的訊息放展開
      const name = `<b>${esc(agentName?.(row.session, row.agentId) ?? String(row.agentId ?? "").slice(0, 8))}</b>`;
      body = row.result === "fail" ? `<span class="st bad">✗ ${name} 失敗</span>`
        : row.result === "killed" ? `<span class="st idle">⏹ ${name} 已停止</span>`
        : `<span class="st agdone">← ${name} 完成</span>`;
      detail = row.text || null;
    } else if (row.status === "session") {
      body = SESSION_TEXT[row.source] ?? `<span class="st">↻ ${esc(row.source)}</span>`;
    } else {
      body = STATUS_TEXT[row.status] ?? esc(row.status);
    }
  } else {
    const c = row.card;
    body = toolBody(c) + statusSuffix(c);
    if (c.danger) {
      cls.push("dz");
      body += ` <span class="warn">⚠ ${esc(c.danger.why ?? c.danger.rule)}</span>`;
    }
    // 背景 agent 本來就沒有 transcript，一律 fallback 進串流（contracts.md §2），不是降級
    if (c.fallback && row.agent !== "bg") tags.push("無 transcript");
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
