// 浮動視窗：不加遮罩、不模糊背景，頁面照常可操作；拖標題列移動，右下角縮放（CSS resize），
// 位置與大小記在這個瀏覽器（localStorage，讀寫失敗就用預設）。

const MIN_W = 280;
const MIN_H = 140;
const GRAB = 48; // 左右至少留這麼寬在畫面內，才抓得回來
const HEAD = 32; // 標題列高度：往下拖時至少留這麼多

const num = (v) => (typeof v === "number" && Number.isFinite(v) ? v : null);

// 夾住位置與大小，讓視窗不會拖到抓不回來；壞掉或缺的值用 fallback
export function clampRect(r, { vw, vh }, fallback = { x: 40, y: 40, w: 520, h: 320 }) {
  const w = Math.min(vw, Math.max(MIN_W, num(r?.w) ?? fallback.w));
  const h = Math.min(vh, Math.max(MIN_H, num(r?.h) ?? fallback.h));
  const x = Math.min(vw - GRAB, Math.max(GRAB - w, num(r?.x) ?? fallback.x));
  const y = Math.min(vh - HEAD, Math.max(0, num(r?.y) ?? fallback.y));
  return { x, y, w, h };
}

export function makeFloating(win, { handle, key, initial, onToggle }) {
  const view = () => ({ vw: window.innerWidth, vh: window.innerHeight });
  const current = () => {
    const b = win.getBoundingClientRect();
    return { x: b.left, y: b.top, w: b.width, h: b.height };
  };
  const apply = (r) => {
    const c = clampRect(r, view(), initial());
    Object.assign(win.style, { left: `${c.x}px`, top: `${c.y}px`, width: `${c.w}px`, height: `${c.h}px` });
  };
  const load = () => {
    try {
      return JSON.parse(localStorage.getItem(key) ?? "null");
    } catch {
      return null;
    }
  };
  const save = () => {
    try {
      localStorage.setItem(key, JSON.stringify(current()));
    } catch {
      // 私密視窗或封鎖儲存：不記也沒關係
    }
  };

  // 拖曳：只從標題列（不含按鈕）開始
  handle.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || e.target.closest("button")) return;
    e.preventDefault();
    const start = current();
    const sx = e.clientX;
    const sy = e.clientY;
    try { handle.setPointerCapture(e.pointerId); } catch { /* 合成事件沒有 pointerId */ }
    const move = (ev) => apply({ ...start, x: start.x + ev.clientX - sx, y: start.y + ev.clientY - sy });
    const up = () => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", up);
      handle.removeEventListener("pointercancel", up);
      save();
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", up);
    handle.addEventListener("pointercancel", up);
  });

  // 縮放（CSS resize）結束後記下來
  let saveTimer = 0;
  if (typeof ResizeObserver === "function") {
    new ResizeObserver(() => {
      if (!win.open) return;
      clearTimeout(saveTimer);
      saveTimer = setTimeout(save, 200);
    }).observe(win);
  }
  // 視窗大小改變時夾回畫面內
  window.addEventListener("resize", () => { if (win.open) apply(current()); });
  // 非 modal 的 dialog 預設不吃 Esc
  win.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      api.close();
    }
  });

  const api = {
    open() {
      if (win.open) return;
      win.show(); // 非 modal：沒有 ::backdrop、頁面照常可操作
      apply(load() ?? initial());
      onToggle?.(true);
    },
    close() {
      if (!win.open) return;
      save();
      win.close();
      onToggle?.(false);
    },
    toggle() {
      if (win.open) api.close();
      else api.open();
    },
    get isOpen() {
      return win.open;
    },
  };
  return api;
}
