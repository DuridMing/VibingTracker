// 通知（PLAN「注意佇列」的「通知」、#647）：頁內 toast、提示音、瀏覽器 Notification，預設全關。
// 這裡只有判斷邏輯（時間、儲存、BroadcastChannel 都注入）；真正出聲、跳通知在 main.js。

import { KIND_ORDER, KIND_TEXT } from "./attention.js";

const KEY = "coding-lab.notify";
export const DEFAULT_SETTINGS = Object.freeze({ toast: false, sound: false, browser: false });

// 開關存 localStorage（每個瀏覽器一份）；讀不到或壞掉就當預設
export function loadSettings(storage) {
  try {
    const v = JSON.parse(storage?.getItem(KEY) ?? "null");
    if (v == null || typeof v !== "object") return { ...DEFAULT_SETTINGS };
    return { toast: v.toast === true, sound: v.sound === true, browser: v.browser === true };
  } catch {
    return { ...DEFAULT_SETTINGS };
  }
}

export function saveSettings(storage, s) {
  try {
    storage?.setItem(KEY, JSON.stringify({ ...DEFAULT_SETTINGS, ...s }));
  } catch {
    // 私密視窗等情況寫不進去：這次開著有效，下次回到預設
  }
}

// toast 是頁內的，每個主視窗各自顯示；聲音與瀏覽器通知同時開多個主視窗時只由 leader 發；監視器視窗都不發
export function sinksFor(settings, { isLeader, isMonitor }) {
  if (isMonitor) return [];
  const out = [];
  if (settings.toast) out.push("toast");
  if (isLeader && settings.sound) out.push("sound");
  if (isLeader && settings.browser) out.push("browser");
  return out;
}

const rank = (k) => KIND_ORDER[k] ?? 99;

// 一批項目合成一則：kind 是最高優先的種類（決定提示音）
export function summarize(items, sessionLabel) {
  const kind = [...items].sort((a, b) => rank(a.kind) - rank(b.kind))[0].kind;
  const text = (k) => KIND_TEXT[k] ?? k;
  if (items.length === 1) {
    const it = items[0];
    return { kind, title: `${text(it.kind)} · ${sessionLabel(it.session)}`, body: it.reason ?? "" };
  }
  const sessions = new Set(items.map((x) => x.session)).size;
  const kinds = [...new Set(items.map((x) => x.kind))].sort((a, b) => rank(a) - rank(b));
  if (kinds.length === 1) {
    return { kind, title: `${sessions} 個 session ${text(kind)}`, body: [...new Set(items.map((x) => sessionLabel(x.session)))].join("、") };
  }
  const count = (k) => items.filter((x) => x.kind === k).length;
  return { kind, title: `${sessions} 個 session 需要注意`, body: kinds.map((k) => `${text(k)} ${count(k)}`).join(" · ") };
}

// 不補響：只通知這次連線之後才 raised、而且 snapshot 裡沒有的項目；1 秒內的多筆合成一則
export function createNotifier({ now, setTimeout, clearTimeout, emit, windowMs = 1000, skewMs = 2000 }) {
  let baseline = Infinity; // 連上之前不通知
  const known = new Set();
  let pending = [];
  let timer = null;

  function reset() {
    if (timer != null) clearTimeout(timer);
    timer = null;
    pending = [];
  }
  function flush() {
    timer = null;
    const items = pending;
    pending = [];
    if (items.length) emit({ items });
  }
  return {
    // 收到 welcome（第一次連上或重連）：之前看過的不算，基準時間重設
    connected() {
      reset();
      known.clear();
      baseline = now();
    },
    // 同一條連線上的 snapshot（例如離開回放時重訂 `*`）：合併中、還在 snapshot 裡的照發，不在的（已確認、已刪）丟掉。
    // 重新連線時 connected() 已經先清掉待發的，所以不會補響（#765 C5）
    snapshot(rows) {
      const ids = new Set();
      for (const r of rows ?? []) if (r?.id != null) ids.add(r.id);
      pending = pending.filter((x) => ids.has(x.id));
      if (!pending.length) reset();
      known.clear();
      for (const id of ids) known.add(id);
    },
    frame(rows) {
      for (const r of rows ?? []) {
        if (r?.id == null) continue;
        if (r.deleted === true) {
          pending = pending.filter((x) => x.id !== r.id);
          continue;
        }
        if (known.has(r.id)) continue;
        known.add(r.id);
        // resume 補送的 frame 可能帶著斷線期間 raised 的項目：比連線時間早的不響（容許 collector 與瀏覽器時鐘差一點）
        if (!(r.raisedAt >= baseline - skewMs)) continue;
        pending.push(r);
        if (timer == null) timer = setTimeout(flush, windowMs);
      }
    },
  };
}

// 多個主視窗用 BroadcastChannel 選一個負責出聲：活著的視窗中，看得到的優先，再取 id 最小的（id 用開啟時間開頭，所以是最早開的那個）。
// 背景分頁的計時器會被瀏覽器節流（心跳可能一分鐘才一次），只比 id 時前景、背景的分頁會都以為自己是 leader（#747 F-3）
export function createLeader({ id, channel, now, setInterval, clearInterval, beatMs = 1500, ttlMs = 5000, visible = () => true }) {
  if (channel == null) return { isLeader: () => true, beat() {}, stop() {} };
  const peers = new Map(); // id → { seen: 最後一次聽到的時間, vis: 那時看不看得到 }
  const beat = () => channel.postMessage({ t: "beat", id, vis: visible() });
  channel.onmessage = (e) => {
    const m = e.data;
    if (m == null || m.id === id) return;
    if (m.t === "bye") return void peers.delete(m.id);
    if (m.t !== "beat") return;
    const isNew = !peers.has(m.id);
    peers.set(m.id, { seen: now(), vis: m.vis !== false });
    if (isNew) beat(); // 新開的視窗馬上知道有誰在
  };
  const timer = setInterval(beat, beatMs);
  beat();
  return {
    isLeader() {
      const t = now();
      const me = visible();
      let lead = true;
      for (const [p, x] of peers) {
        // 背景分頁的心跳稀疏：看得到的同伴照 ttl 判斷，看不到的同伴放寬到一分鐘多
        if (t - x.seen > (x.vis ? ttlMs : Math.max(ttlMs, 65_000))) peers.delete(p);
        else if (x.vis && !me) lead = false;
        else if (x.vis === me && p < id) lead = false;
      }
      return lead;
    },
    // 看得見與否變了（visibilitychange）時馬上告訴其他視窗
    beat,
    stop() {
      clearInterval(timer);
      channel.postMessage({ t: "bye", id });
      channel.close?.();
    },
  };
}
