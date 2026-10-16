// 合流串流的虛擬化渲染：只畫看得到的行（加上下緩衝），行用 translateY 定位。
// 每行固定 ROW_H 高；展開的行量一次高度記在 row.h。資料與排序在 stream.js。
// 「已顯示的行位置永遠不變」：上面的行被丟掉或補進來時，同步調整 scrollTop。

import { layout, indexAt } from "./vlist.js";
import { formatRow, esc } from "./format.js";

export const ROW_H = 20;
const OVERSCAN = 30;

export function createStreamView({ scroller, model, agentName, onNeedMore, onFollow, prof = null }) {
  const T = () => (prof ? performance.now() : 0);
  const more = document.createElement("button");
  more.className = "vl-more";
  more.textContent = "↑ 載入更多";
  more.hidden = true;
  const space = document.createElement("div");
  space.className = "vl-space";
  const pill = document.createElement("button");
  pill.className = "newpill";
  pill.textContent = "↓ 新事件";
  scroller.replaceChildren(more, space, pill);

  const nodes = new Map(); // row.key → { el, row, v, open }
  let top = layout([], () => 0);
  let dirty = true;
  let follow = true;
  let unseen = 0;
  let raf = 0;
  let pendingShift = 0; // 下次 render 時要加到 scrollTop 的量（上方丟掉為負、補進為正）
  let anchor = null; // 篩選改變時要維持在原位置的行
  // 幾何資訊快取起來，render 時不讀 DOM（讀 scrollHeight／offsetTop 會強制排版）
  const geo = { off: 0, vh: 0, stale: true };
  const readGeo = () => {
    geo.off = space.offsetTop;
    geo.vh = scroller.clientHeight;
    geo.stale = false;
  };
  if (typeof ResizeObserver === "function") new ResizeObserver(() => { geo.stale = true; schedule(); }).observe(scroller);

  const heightOf = (r) => (model.isVisible(r) ? (r.expanded && r.h ? r.h : ROW_H) : 0);
  const atBottom = () => scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - ROW_H;

  function setFollow(on) {
    if (follow === on) return;
    follow = on;
    if (on) {
      unseen = 0;
      pill.classList.remove("show");
    }
    onFollow?.(on);
  }

  function schedule() {
    if (!raf) raf = requestAnimationFrame(render);
  }

  scroller.addEventListener("scroll", () => {
    setFollow(atBottom());
    schedule();
  }, { passive: true });

  pill.addEventListener("click", () => {
    setFollow(true);
    scroller.scrollTop = scroller.scrollHeight;
    schedule();
  });
  more.addEventListener("click", () => onNeedMore?.());

  function toggle(el) {
    const nd = nodes.get(el.dataset.key);
    if (!nd) return;
    nd.row.expanded = !nd.row.expanded;
    nd.row.h = 0;
    nd.row.v++;
    dirty = true;
    schedule();
  }
  space.addEventListener("click", (e) => {
    if (e.target.closest(".full")) return; // 展開內容裡可以選字，不收合
    const el = e.target.closest(".ln.can-open");
    if (el) toggle(el);
  });
  // 鍵盤：可展開的行可以用 Tab 選到，Enter／空白鍵展開或收合
  space.addEventListener("keydown", (e) => {
    if (e.key !== "Enter" && e.key !== " ") return;
    const el = e.target.closest(".ln.can-open");
    if (!el || e.target !== el) return;
    e.preventDefault();
    toggle(el);
  });

  function paint(nd) {
    const r = nd.row;
    const f = formatRow(r, { agentName });
    nd.el.className = f.cls;
    nd.el.dataset.key = r.key;
    if (f.detail != null) {
      nd.el.tabIndex = 0;
      nd.el.setAttribute("role", "button");
      nd.el.setAttribute("aria-expanded", String(r.expanded));
    } else {
      nd.el.removeAttribute("tabindex");
      nd.el.removeAttribute("role");
      nd.el.removeAttribute("aria-expanded");
    }
    nd.el.innerHTML = `<div class="hd">${f.html}</div>` + (r.expanded && f.detail != null ? `<div class="full">${esc(f.detail)}</div>` : "");
    nd.v = r.v;
    nd.open = r.expanded;
  }

  function render() {
    raf = 0;
    const t0 = T();
    if (geo.stale) readGeo();
    const rows = model.rows;
    const n = rows.length;
    if (anchor) {
      // 篩選前，畫面頂端那一行的位置；篩選後把它放回同樣的位置
      const idx = rows.findIndex((r) => r.key === anchor.key);
      top = layout(rows, heightOf);
      dirty = false;
      space.style.height = `${top[n]}px`;
      if (idx >= 0 && !follow) scroller.scrollTop = geo.off + top[idx] - anchor.delta;
      anchor = null;
    } else if (dirty) {
      top = layout(rows, heightOf);
      dirty = false;
      space.style.height = `${top[n]}px`;
    }
    let y0;
    if (follow) {
      // 跟隨時不讀 scrollHeight：設一個很大的值讓瀏覽器夾到底，可見範圍由自己算
      scroller.scrollTop = 1e9;
      y0 = Math.max(0, top[n] + geo.off - geo.vh);
      pendingShift = 0;
    } else {
      if (pendingShift) scroller.scrollTop += pendingShift;
      pendingShift = 0;
      y0 = scroller.scrollTop - geo.off;
    }
    const t1 = T();
    const y1 = y0 + geo.vh;
    let painted = 0;
    const want = new Set();
    if (n > 0) {
      const i0 = Math.max(0, indexAt(top, y0) - OVERSCAN);
      const i1 = Math.min(n - 1, indexAt(top, y1) + OVERSCAN);
      for (let i = i0; i <= i1; i++) {
        const r = rows[i];
        if (top[i + 1] === top[i]) continue; // 被篩掉
        want.add(r.key);
        let nd = nodes.get(r.key);
        if (!nd) {
          nd = { el: document.createElement("div"), row: r, v: -1, open: false };
          space.appendChild(nd.el);
          nodes.set(r.key, nd);
        }
        nd.row = r;
        if (nd.v !== r.v || nd.open !== r.expanded) { paint(nd); painted++; }
        nd.el.style.transform = `translateY(${top[i]}px)`;
      }
    }
    for (const [k, nd] of nodes) {
      if (!want.has(k)) {
        nd.el.remove();
        nodes.delete(k);
      }
    }
    if (prof) { const t2 = T(); prof.push({ pre: t1 - t0, dom: t2 - t1, painted, nodes: nodes.size }); }
    // 展開的行量一次高度；量到之後重排一次
    let remeasure = false;
    for (const nd of nodes.values()) {
      if (nd.row.expanded && !nd.row.h) {
        nd.row.h = Math.max(ROW_H, nd.el.offsetHeight);
        remeasure = true;
      }
    }
    if (remeasure) {
      dirty = true;
      render();
    }
  }

  return {
    // model.apply／trim 之後呼叫
    update(ops) {
      if (ops.droppedRows?.length) {
        for (const r of ops.droppedRows) pendingShift -= heightOf(r);
        for (const r of ops.droppedRows) {
          const nd = nodes.get(r.key);
          if (nd) { nd.el.remove(); nodes.delete(r.key); }
        }
      }
      for (const k of ops.updated ?? []) {
        const nd = nodes.get(k);
        if (nd) nd.v = -1;
      }
      if (ops.appended && !follow) {
        unseen += ops.appended;
        pill.textContent = `↓ ${unseen} 個新事件`;
        pill.classList.add("show");
      }
      dirty = true;
      schedule();
    },
    // model.prepend 之後呼叫
    prepended(res) {
      for (const r of res.rows) pendingShift += heightOf(r);
      dirty = true;
      schedule();
    },
    // snapshot 之後整個重來
    reset() {
      for (const nd of nodes.values()) nd.el.remove();
      nodes.clear();
      pendingShift = 0;
      setFollow(true);
      dirty = true;
      schedule();
    },
    // 篩選改變：維持畫面頂端那一行的位置
    filterChanged() {
      if (!follow && model.rows.length) {
        const y = scroller.scrollTop - geo.off;
        let i = indexAt(top, y);
        while (i < model.rows.length && top[i + 1] === top[i]) i++;
        if (i < model.rows.length) anchor = { key: model.rows[i].key, delta: top[i] - y };
      }
      for (const nd of nodes.values()) nd.v = -1;
      dirty = true;
      schedule();
    },
    setHasMore(on) {
      if (more.hidden === !on) return;
      more.hidden = !on;
      geo.stale = true; // 按鈕出現／消失會改變串流內容的起點
      schedule();
    },
    // 立即渲染（效能量測用）
    flush() {
      if (raf) cancelAnimationFrame(raf);
      render();
    },
    get following() {
      return follow;
    },
  };
}
