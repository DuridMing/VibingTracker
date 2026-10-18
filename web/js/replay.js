// 專案回放的分頁與時間範圍（#702；#730 起回放在主頁上進行，時間軸在 timeline.js）。

export const REPLAY_TAB = "replay";

const H = 3_600_000;
const DAY = 24 * H;

// 「今天」「昨天」用使用者時區的午夜（時間一律存 UTC，只在前端轉時區）
export function rangeFor(choice, now, utcOffsetMin = 0) {
  const off = utcOffsetMin * 60_000;
  const midnight = Math.floor((now + off) / DAY) * DAY - off;
  if (choice === "today") return { from: midnight, to: now };
  if (choice === "yesterday") return { from: midnight - DAY, to: midnight - 1 };
  return { from: now - 3 * H, to: now };
}
