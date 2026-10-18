// 主頁時間軸回放（#730，不碰 DOM）：選專案與時間範圍後，主頁的每個面板改吃 GET /api/projects/replay
// （collector 把記錄下來的 Event 重放成 T 時刻的 snapshot 與之後逐時間點的 frame，#729）。
// 這裡只管時間：目前時刻 t、播放與速度、哪些 frame 到點了、什麼時候要下一段、哪個回應已經過期。
// 套用沿用即時的路徑（applySnapshot／applyFrame／串流／改動紀錄），回放中擋下即時的 `*`，離開時重訂 `*` 拿新的 snapshot。

export const SPEEDS = [1, 10, 60, 600];
const LIMIT = 2000;
// 佇列剩不到這麼多個 frame 就先要下一段，播放不會停在段與段之間
const LOW_WATER = 200;

// from：續頁沿用第一段回應的下界（伺服器挑 session 用，跨午夜時才不會少算，#742 C6）
export function timelineUrl({ cwd, at, until, offset = 0, snapshot = true, limit = LIMIT, from = null }) {
  let url = `/api/projects/replay?cwd=${encodeURIComponent(cwd)}&at=${at}&until=${until}&offset=${offset}&limit=${limit}`;
  if (!snapshot) url += "&snapshot=0";
  if (from != null) url += `&from=${from}`;
  return url;
}

export function createTimeline() {
  const tl = {
    active: false, cwd: null, from: 0, to: 0, t: 0,
    playing: false, speed: 60, ended: false,
    queue: [], next: null, loading: false, req: 0, error: null,
    lower: null, // 伺服器回的挑 session 下界（回應的 from），續頁帶回去
  };
  const clamp = (t) => Math.min(tl.to, Math.max(tl.from, t));

  // 回放中：即時的 `*`（snapshot、frame）不碰主頁，離開回放時重訂 `*` 換回現況
  tl.blocksLive = () => tl.active;

  tl.start = ({ cwd, from, to }) => {
    Object.assign(tl, { active: true, cwd, from, to, t: from, playing: false, ended: false, queue: [], next: null, loading: false, error: null });
  };
  tl.stop = () => {
    tl.req++;
    Object.assign(tl, { active: false, playing: false, queue: [], next: null, loading: false });
  };

  // 跳到某個時刻（拖動、重新載入）：舊段作廢；回傳這次請求的編號，回應帶回來時比對
  tl.seek = (t) => {
    tl.req++;
    Object.assign(tl, { t: clamp(t), queue: [], next: null, loading: true, ended: false, error: null, lower: null });
    return tl.req;
  };
  // seek 的回應：過期回 null；否則換上這段的 frame，回傳要套用的 snapshot
  tl.loaded = (req, body) => {
    if (req !== tl.req) return null;
    tl.queue = [...(body.frames ?? [])];
    tl.next = body.next ?? null;
    tl.lower = body.from ?? null;
    tl.loading = false;
    return body.snapshot ?? null;
  };
  tl.failed = (req, message) => {
    if (req !== tl.req) return;
    tl.loading = false;
    tl.playing = false;
    tl.error = message;
  };

  tl.needsMore = () => tl.active && !tl.loading && tl.next != null && tl.queue.length < LOW_WATER;
  // 開始要下一段（at=next、不要 snapshot）；回傳請求編號
  tl.loadingMore = () => {
    tl.loading = true;
    return tl.req;
  };
  tl.appended = (req, body) => {
    if (req !== tl.req) return false;
    tl.queue.push(...(body.frames ?? []));
    tl.next = body.next ?? null;
    tl.loading = false;
    return true;
  };

  tl.play = (speed = tl.speed) => {
    tl.speed = speed;
    // 讀取失敗（沒有資料）不假裝在播；重新 seek 成功會清掉 error（#765 C14）
    if (tl.t >= tl.to || tl.error) return;
    tl.playing = true;
    tl.ended = false;
  };
  tl.pause = () => { tl.playing = false; };

  // 經過 wallMs 毫秒（實際時間）：播放中依速度前進，交出到點的 frame（依序，套用的人照這個順序套）。
  // 還有下一段（next 不是 null）時，時鐘最多走到已載入的最後一個時間點 next，等下一段到了再走；
  // 不然高倍速下時鐘會先跑到結尾、晚到的那段永遠套不到（#765 C4）
  tl.tick = (wallMs) => {
    if (!tl.active || !tl.playing) return [];
    const limit = tl.next != null ? Math.min(tl.next, tl.to) : tl.to;
    tl.t = Math.max(tl.t, Math.min(clamp(tl.t + wallMs * tl.speed), limit));
    let n = 0;
    while (n < tl.queue.length && tl.queue[n].ts <= tl.t) n++;
    const due = tl.queue.splice(0, n);
    if (tl.next == null && tl.t >= tl.to) {
      tl.playing = false;
      tl.ended = true;
    }
    return due;
  };
  return tl;
}
