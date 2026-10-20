// 專案回放的分頁與時間範圍（#702；#730 起回放在主頁上進行，時間軸在 timeline.js）。

export const REPLAY_TAB = "replay";

const H = 3_600_000;
const DAY = 24 * H;

// 「今天」「昨天」用使用者時區的午夜（時間一律存 UTC，只在前端轉時區）
// utcOffsetMin：固定的位移（分），或依時刻回傳位移的函式。夏令時間切換那天午夜的位移和現在不同，
// 要用那個時刻的位移算，「今天」「昨天」才不會差一小時（#747 F-13）
export function rangeFor(choice, now, utcOffsetMin = 0) {
  const offAt = typeof utcOffsetMin === "function" ? utcOffsetMin : () => utcOffsetMin;
  const midnightOf = (t) => {
    const day = Math.floor((t + offAt(t) * 60_000) / DAY) * DAY; // 當地日期的 00:00（當成 UTC 寫）
    return day - offAt(day - offAt(t) * 60_000) * 60_000;
  };
  const midnight = midnightOf(now);
  if (choice === "today") return { from: midnight, to: now };
  if (choice === "yesterday") return { from: midnightOf(midnight - 1), to: midnight - 1 };
  return { from: now - 3 * H, to: now };
}
