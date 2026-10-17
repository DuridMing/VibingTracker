// 全專案回放（#702，不碰 DOM）：選一個專案與時間範圍，從 GET /api/projects/events 由新往舊一頁一頁讀，
// 所有 session（含已結束）合成一條串流。不接即時更新。
// 順序沿用合流串流的規則：同 session 依 pos，跨 session 依 (ts_seen, session, pos)（contracts §6），由 stream.prepend 合併。

import { createStream } from "./stream.js";

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

export function replayUrl({ cwd, from, to, before = null, limit = 300 }) {
  let url = `/api/projects/events?cwd=${encodeURIComponent(cwd)}&from=${from}&to=${to}&limit=${limit}`;
  if (before) url += `&before=${encodeURIComponent(`${before.ts},${before.session},${before.pos}`)}`;
  return url;
}

export function createReplay() {
  // stream 物件不換：畫面那層（stream-view）只建一次，拿著同一個 model
  const r = { stream: createStream(), cwd: null, from: 0, to: 0, next: null, done: false, loading: false, error: null };
  r.reset = ({ cwd, from, to }) => {
    r.stream.load({});
    r.stream.hiddenSessions.clear();
    Object.assign(r, { cwd, from, to, next: null, done: false, loading: false, error: null });
  };
  // 一頁 API 回應（比畫面上的都舊）：server 依 session 分段，每段已依 pos 排好、卡片的狀態與結果跟在後面
  r.load = (page) => {
    const bySession = new Map();
    for (const e of page.events ?? []) {
      if (!bySession.has(e.session)) bySession.set(e.session, []);
      bySession.get(e.session).push(e);
    }
    const ops = r.stream.prepend([...bySession.values()]);
    r.next = page.next ?? null;
    r.done = r.next == null;
    r.loading = false;
    return ops;
  };
  // 出現在回放裡的 session（session 篩選用）
  r.sessions = () => [...new Set(r.stream.rows.map((x) => x.session))].sort();
  r.reset({ cwd: null, from: 0, to: 0 });
  return r;
}
