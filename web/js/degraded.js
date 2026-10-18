// transcript 降級的標語（#652）：collector 的 sessions 列帶 degraded（降級中的檔），這裡只決定怎麼說。

import { shortId as short } from "./color.js";

/** @returns {{sessions: string[], text: string} | null} */
export function degradedNotice(sessions) {
  const hit = [...sessions].filter((s) => s.status !== "ended" && Array.isArray(s.degraded) && s.degraded.length > 0);
  if (hit.length === 0) return null;
  const onlyAgents = hit.every((s) => s.degraded.every((f) => f !== "main"));
  const who = hit.map((s) => short(s.id)).join("、");
  const scope = onlyAgents ? "（subagent 的 transcript）" : "";
  return {
    sessions: hit.map((s) => s.id),
    text: `${who} 的 transcript 讀不到${scope}：文字與思考不可用，只顯示工具事件`,
  };
}
