// 改動紀錄（不碰 DOM）。規則見 PLAN.md「前端狀態機與合流串流」：
// - 跨 session 的獨立清單，最新在上，最多 60 筆。
// - 新改動進來自動選中；點舊的一筆就停住，直到手動選最新。
// - 被拒或失敗的 Edit／Write（同一張卡片的 tool.updated failed）視為沒有發生，從清單拿掉（contracts §4，#682）。
// diff 以資料存（hunk／全文），顯示時才由 diffRows／writeRows 轉成行。

export const CAP = 60;

function entryOf(ev) {
  const p = ev.payload ?? {};
  const base = { key: `${ev.session}:${ev.seq}`, session: ev.session, agent: ev.agent, ts: ev.ts_seen, toolUseId: p.toolUseId, path: p.path ?? "" };
  if (ev.type === "file.edit") {
    let added = 0;
    let removed = 0;
    for (const l of p.hunk?.lines ?? []) {
      if (l.startsWith("+")) added++;
      else if (l.startsWith("-")) removed++;
    }
    return { ...base, kind: "edit", hunk: p.hunk ?? null, hunkSource: p.hunkSource ?? "none", added, removed };
  }
  if (ev.type === "file.write") {
    return { ...base, kind: "write", isNew: !!p.isNew, content: p.content ?? "", truncated: !!p.truncated };
  }
  return null;
}

export function createChanges({ cap = CAP } = {}) {
  const ch = { list: [], following: true, selectedKey: null };
  const keys = new Set();
  const failed = new Set(); // 以 failed 結束的 toolUseId（失敗可能比 file.* 先到）

  function isFailure(ev) {
    return ev.type === "tool.updated" && ev.payload?.status === "failed" && ev.payload?.toolUseId != null;
  }

  // 拿掉這張卡片的改動；回傳是否有拿掉
  function drop(id) {
    failed.add(id);
    const before = ch.list.length;
    ch.list = ch.list.filter((c) => {
      if (c.toolUseId !== id) return true;
      keys.delete(c.key);
      return false;
    });
    return ch.list.length !== before;
  }

  function add(ev) {
    const e = entryOf(ev);
    if (!e || keys.has(e.key) || failed.has(e.toolUseId)) return false;
    keys.add(e.key);
    ch.list.unshift(e);
    return true;
  }

  // 選中的那筆被拿掉：跟著最新時選最新，停住時改選最接近的（留下來最新的一筆）
  function reselect() {
    if (keys.has(ch.selectedKey)) return;
    ch.selectedKey = ch.list[0]?.key ?? null;
  }

  function trim() {
    while (ch.list.length > cap) keys.delete(ch.list.pop().key);
    // 停住時選中的那筆被擠出去：改選留下來最舊的一筆，仍然停住
    if (!ch.following && !keys.has(ch.selectedKey)) ch.selectedKey = ch.list.at(-1)?.key ?? null;
  }

  // 回傳是否有新改動
  ch.apply = (events) => {
    let added = false;
    let dropped = false;
    for (const ev of events ?? []) {
      if (isFailure(ev)) dropped = drop(ev.payload.toolUseId) || dropped;
      else if (add(ev)) added = true;
    }
    if (!added && !dropped) return false;
    trim();
    if (ch.following) ch.selectedKey = ch.list[0]?.key ?? null;
    else reselect();
    return true;
  };

  // snapshot.changes：舊到新
  ch.load = (events) => {
    ch.list = [];
    keys.clear();
    failed.clear();
    for (const ev of events ?? []) if (isFailure(ev)) failed.add(ev.payload.toolUseId);
    for (const ev of events ?? []) if (!isFailure(ev)) add(ev);
    trim();
    ch.following = true;
    ch.selectedKey = ch.list[0]?.key ?? null;
  };

  // 載入更多／回放：比現有的都舊的一段（舊到新），接在清單尾端，不改選取
  ch.prepend = (events) => {
    const older = [];
    for (const ev of events ?? []) {
      const e = entryOf(ev);
      if (!e || keys.has(e.key)) continue;
      keys.add(e.key);
      older.unshift(e);
    }
    ch.list.push(...older);
    trim();
    if (ch.following) ch.selectedKey = ch.list[0]?.key ?? null;
    return older.length > 0;
  };

  ch.select = (key) => {
    if (!keys.has(key)) return;
    ch.selectedKey = key;
    ch.following = key === ch.list[0]?.key;
  };

  ch.selected = () => ch.list.find((c) => c.key === ch.selectedKey) ?? null;

  return ch;
}

// unified diff 的行：雙行號。hunk 的 start 同時是舊檔與新檔的起始行（hunk 前面的行沒有變）。
export function diffRows(hunk, hunkSource = "none") {
  if (!hunk) {
    const why = hunkSource === "none" ? "檔案太大或讀不到，沒有 diff" : "沒有 diff";
    return [{ type: "note", text: why }];
  }
  const rows = [{ type: "hunk", text: `@@ -${hunk.start},${hunk.oldLines} +${hunk.start},${hunk.newLines} @@` }];
  let o = hunk.start;
  let n = hunk.start;
  for (const l of hunk.lines ?? []) {
    const mark = l[0];
    const text = l.slice(1);
    if (mark === "+") rows.push({ type: "add", old: null, new: n++, text });
    else if (mark === "-") rows.push({ type: "del", old: o++, new: null, text });
    else rows.push({ type: "ctx", old: o++, new: n++, text });
  }
  return rows;
}

// Write 全文（collector 已截 200 行）
export function writeRows(content, truncated) {
  const lines = String(content ?? "").replace(/\n$/, "").split("\n");
  const rows = lines.map((text, i) => ({ type: "line", new: i + 1, text }));
  if (truncated) rows.push({ type: "note", text: "…只顯示前 200 行，全文在 transcript" });
  return rows;
}
