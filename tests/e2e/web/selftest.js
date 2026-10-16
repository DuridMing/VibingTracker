// 前端互動自我測試：只由 shotserver.py 注入截圖頁（/shot.html?…&selftest）。
// 用真的 DOM 點擊、捲動、按鍵操作合流串流、改動紀錄、「即時」浮動視窗；
// 結果畫在左上角，並 POST 到 /__result 讓 tests/scripts/web-selftest.sh 讀回判定。
// 需要 mock 有歷史資料：?mock=16&prefill&selftest。
(() => {
  const out = [];
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const $ = (s) => document.querySelector(s);
  const $$ = (s) => [...document.querySelectorAll(s)];
  const sc = () => $("#stream");
  const rows = () => $$("#stream .vl-space .ln");
  const show = () => {
    let el = $("#selftest");
    if (!el) {
      el = document.createElement("pre");
      el.id = "selftest";
      el.style.cssText = "position:fixed;left:12px;top:12px;z-index:99;margin:0;padding:10px 14px;max-width:760px;white-space:pre-wrap;background:#111;color:#eee;font:12px/1.5 monospace;border-radius:8px";
      document.body.appendChild(el);
    }
    el.textContent = out.join("\n");
  };
  const check = (name, ok, info = "") => { out.push(`${ok ? "PASS" : "FAIL"}  ${name}${info ? "  — " + info : ""}`); show(); };
  const click = (el) => el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  const visibleTop = () => {
    // 畫面上第一個完整可見的行與它在畫面上的 y
    const box = sc().getBoundingClientRect();
    let best = null;
    for (const el of rows()) {
      const r = el.getBoundingClientRect();
      if (r.top >= box.top + 4 && (!best || r.top < best.y)) best = { key: el.dataset.key, y: r.top, el };
    }
    return best;
  };
  const yOf = (key) => {
    const el = rows().find((e) => e.dataset.key === key);
    return el ? el.getBoundingClientRect().top : null;
  };
  const scrollMid = async () => {
    sc().scrollTop = sc().scrollHeight / 2;
    sc().dispatchEvent(new Event("scroll"));
    await sleep(120);
  };

  async function run() {
    for (let i = 0; i < 100 && rows().length < 10; i++) await sleep(100);
    out.push(`rows in DOM: ${rows().length}`);

    // 1. 跟隨：在底部
    const s = sc();
    check("一開始跟隨在底部", s.scrollTop + s.clientHeight >= s.scrollHeight - 24, `scrollTop=${Math.round(s.scrollTop)} h=${s.scrollHeight}`);

    // 2. 展開／收合
    await scrollMid();
    const opener = rows().find((e) => e.classList.contains("can-open") && e.getBoundingClientRect().top > s.getBoundingClientRect().top + 40);
    if (opener) {
      const key = opener.dataset.key;
      const next = rows().find((e) => e.getBoundingClientRect().top > opener.getBoundingClientRect().top + 5);
      const nextKey = next?.dataset.key;
      const nextY0 = nextKey ? yOf(nextKey) : null;
      click(opener.querySelector(".hd"));
      await sleep(150);
      const el = rows().find((e) => e.dataset.key === key);
      const full = el?.querySelector(".full");
      check("點一下展開全文", !!full && el.classList.contains("open"), full ? `高度 ${Math.round(el.getBoundingClientRect().height)}px` : "沒有 .full");
      const nextY1 = nextKey ? yOf(nextKey) : null;
      check("展開後下面的行往下移、不重疊", nextY0 != null && nextY1 != null && nextY1 >= nextY0 + 20, `下一行 y ${nextY0?.toFixed(0)} → ${nextY1?.toFixed(0)}`);
      click(el.querySelector(".hd"));
      await sleep(150);
      const el2 = rows().find((e) => e.dataset.key === key);
      check("再點一下收合", el2 && !el2.classList.contains("open") && !el2.querySelector(".full"));
    } else check("點一下展開全文", false, "畫面上沒有可展開的行");

    // 3. 篩選類型：思考
    const thChip = $$("#fKind .fchip").find((b) => b.textContent === "思考");
    click(thChip);
    await sleep(350);
    check("隱藏「思考」後畫面上沒有思考行", !rows().some((e) => e.classList.contains("th")), `chip off=${thChip.classList.contains("off")}`);
    click($$("#fKind .fchip").find((b) => b.textContent === "思考"));
    await sleep(350);

    // 4. 往上捲時篩選 session：畫面頂端那一行留在原位
    await scrollMid();
    const top = visibleTop();
    const sessOfTop = top?.el.querySelector(".ss")?.title;
    const other = $$("#fSess .fchip").find((b) => !b.title.startsWith(sessOfTop ?? "-"));
    click(other);
    await sleep(350);
    const y1 = top ? yOf(top.key) : null;
    check("往上捲時篩選 session，頂端那一行不跳", top && y1 != null && Math.abs(y1 - top.y) <= 2, `y ${top?.y.toFixed(0)} → ${y1?.toFixed(0)}`);
    click($$("#fSess .fchip").find((b) => b.classList.contains("off")));
    await sleep(350);

    // 5. 往上捲時有新事件：出現「↓ 新事件」，畫面上的行不動
    await scrollMid();
    const t2 = visibleTop();
    await sleep(2500);
    const pill = $(".newpill");
    check("離開底部時出現「↓ N 個新事件」", pill.classList.contains("show"), pill.textContent);
    const y2 = t2 ? yOf(t2.key) : null;
    check("新事件進來（含上限丟舊行）時畫面上的行不跳", t2 && y2 != null && Math.abs(y2 - t2.y) <= 2, `y ${t2?.y.toFixed(0)} → ${y2?.toFixed(0)}`);
    click(pill);
    await sleep(300);
    check("點「新事件」回到底部、恢復跟隨", !pill.classList.contains("show") && s.scrollTop + s.clientHeight >= s.scrollHeight - 24);

    // 6. 載入更多
    s.scrollTop = 0;
    s.dispatchEvent(new Event("scroll"));
    await sleep(200);
    const more = $(".vl-more");
    if (more && !more.hidden) {
      const t3 = visibleTop();
      const n0 = document.querySelector("#stream .vl-space").getBoundingClientRect().height;
      click(more);
      await sleep(1200);
      const n1 = document.querySelector("#stream .vl-space").getBoundingClientRect().height;
      check("載入更多：上方多了舊行", n1 > n0, `串流高度 ${Math.round(n0)} → ${Math.round(n1)}px`);
      const y3 = t3 ? yOf(t3.key) : null;
      check("載入更多後原本的行留在原位", t3 && y3 != null && Math.abs(y3 - t3.y) <= 2, `y ${t3?.y.toFixed(0)} → ${y3?.toFixed(0)}`);
      s.scrollTop = 1e9;
      s.dispatchEvent(new Event("scroll"));
      await sleep(600);
      const n2 = document.querySelector("#stream .vl-space").getBoundingClientRect().height;
      check("回到底部後修剪回上限（3000 行 × 20px）", n2 <= 3000 * 20 + 1 && (n1 <= 3000 * 20 + 1 || n2 < n1), `串流高度 ${Math.round(n1)} → ${Math.round(n2)}px`);
    } else check("載入更多按鈕出現在最上面", false, "按鈕沒有出現");

    // 7. 暫存區：浮動視窗（不擋頁面、可拖動縮放）
    const win = $("#liveWin");
    click($("#liveToggle"));
    await sleep(400);
    check("點「即時」打開浮動視窗、有內容", win.open && $("#liveList").children.length > 0, `${$("#liveList").children.length} 筆，標題「${$("#liveWinTitle").textContent}」`);
    check("不是 modal：沒有遮罩", !win.matches(":modal"), `resize=${getComputedStyle(win).resize}`);
    // 開著時頁面照常可操作：點類型篩選有反應
    const chip = $$("#fKind .fchip").find((b) => b.textContent === "讀");
    click(chip);
    await sleep(300);
    const chipOff = $$("#fKind .fchip").find((b) => b.textContent === "讀").classList.contains("off");
    click($$("#fKind .fchip").find((b) => b.textContent === "讀"));
    await sleep(300);
    check("視窗開著時頁面照常可操作", chipOff && win.open);
    const r0 = win.getBoundingClientRect();
    const hd = $("#liveWinHandle");
    const P = (type, x, y) => new PointerEvent(type, { bubbles: true, button: 0, clientX: x, clientY: y, pointerId: 1 });
    hd.dispatchEvent(P("pointerdown", r0.left + 60, r0.top + 10));
    hd.dispatchEvent(P("pointermove", r0.left + 60 - 200, r0.top + 10 - 120));
    hd.dispatchEvent(P("pointerup", r0.left + 60 - 200, r0.top + 10 - 120));
    await sleep(100);
    const r1 = win.getBoundingClientRect();
    check("拖標題列移動視窗", Math.round(r0.left - r1.left) === 200 && Math.round(r0.top - r1.top) === 120, `(${Math.round(r0.left)},${Math.round(r0.top)}) → (${Math.round(r1.left)},${Math.round(r1.top)})`);
    check("可縮放（resize: both）", getComputedStyle(win).resize === "both");
    win.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    await sleep(100);
    check("Esc 關閉", !win.open);
    click($("#liveToggle"));
    await sleep(300);
    const r2 = win.getBoundingClientRect();
    check("再打開回到上次的位置", Math.round(r2.left) === Math.round(r1.left) && Math.round(r2.top) === Math.round(r1.top));
    click($("#liveWinClose"));
    await sleep(100);
    check("× 關閉", !win.open);

    // 8. 鍵盤：可展開的行能取得焦點，Enter 展開、再按收合
    await scrollMid();
    const kb = rows().find((e) => e.classList.contains("can-open") && e.getBoundingClientRect().top > s.getBoundingClientRect().top + 40);
    if (kb) {
      const key = kb.dataset.key;
      check("可展開的行有 tabindex 與 role", kb.tabIndex === 0 && kb.getAttribute("role") === "button" && kb.getAttribute("aria-expanded") === "false");
      kb.focus();
      kb.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
      await sleep(150);
      const k1 = rows().find((e) => e.dataset.key === key);
      check("Enter 展開", k1?.classList.contains("open") && k1.getAttribute("aria-expanded") === "true" && document.activeElement === k1);
      k1.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
      await sleep(150);
      check("再按 Enter 收合", !rows().find((e) => e.dataset.key === key)?.classList.contains("open"));
    } else check("鍵盤展開", false, "沒有可展開的行");

    // 9. 改動紀錄與修改情形
    const chgBtns = () => $$("#chg button:not(.hint)");
    for (let i = 0; i < 50 && chgBtns().length < 3; i++) await sleep(100);
    const first = chgBtns()[0];
    check("改動紀錄有資料、最新那筆被選中", first?.classList.contains("sel"), `${chgBtns().length} 筆`);
    const body = $("#fbBody");
    check("修改情形顯示 diff 或全文", !$("#gfb").hidden && body.children.length > 0, `${$("#fbKind").textContent} ${$("#fbPath").textContent}`);
    const editBtn = chgBtns().find((b) => b.querySelector(".k.e"));
    if (editBtn) {
      click(editBtn);
      await sleep(150);
      const hunk = body.querySelector(".hunk")?.textContent ?? "";
      const nums = [...body.querySelectorAll(".dl")].map((d) => [...d.querySelectorAll(".no")].map((n) => n.textContent).join("/"));
      check("Edit 顯示 hunk 標頭與雙行號", /^@@ -\d+,\d+ \+\d+,\d+ @@$/.test(hunk) && nums.length > 0, `${hunk} ${nums.join(" ")}`);
    }
    const older = chgBtns()[2];
    const olderKey = older?.title;
    click(older);
    await sleep(150);
    const pathNow = $("#fbPath").textContent;
    await sleep(2500);
    check("點舊的一筆就停住：新改動進來不切換", $("#fbPath").textContent === pathNow && !!$("#chg .hint"), `仍是 ${pathNow}`);
    click($("#chg .hint"));
    await sleep(150);
    check("點「回到最新」恢復自動", chgBtns()[0]?.classList.contains("sel") && !$("#chg .hint"));

    // ---- 左欄：檔案樹（#642）與改動基準（#644） ----
    const nodes = () => $$("#tree .node");
    const nodeOf = (path) => nodes().find((n) => n.title.split("\n")[0] === path);
    check("檔案樹有節點、目錄在前", nodes().length > 3 && nodes()[0].classList.contains("dir"), `${nodes().length} 列`);
    const touched = nodes().filter((n) => n.querySelector(".mk i"));
    check("碰過的檔有方塊、所在目錄已展開", touched.length > 0, touched.slice(0, 3).map((n) => n.title.split("\n")[0]).join("、"));
    const docs = nodeOf("docs");
    if (docs) {
      // 樹是虛擬化的（只畫看得到的列），所以看子節點出不出現，不數列數
      click(docs);
      await sleep(400);
      const opened = nodeOf("docs")?.getAttribute("aria-expanded") === "true" && !!nodeOf("docs/guide.md");
      click(nodeOf("docs"));
      await sleep(400);
      check("點目錄展開、再點收起", opened && nodeOf("docs")?.getAttribute("aria-expanded") === "false" && !nodeOf("docs/guide.md"));
    } else check("點目錄展開、再點收起", false, "找不到 docs/");
    await sleep(3500); // 等改動後的重抓
    check("相對 main：有改動的檔名上色、說明列有數字", $$("#tree .node.g-mod, #tree .node.g-new").length > 0 && /\d+ 個檔有改動/.test($("#tInfo").textContent), $("#tInfo").textContent);
    const base = $("#tBase");
    base.value = "since";
    base.dispatchEvent(new Event("change"));
    await sleep(400);
    check("切到「本次開啟以來」：不問 collector，用收到的改動", /^本次開啟以來 · \d+ 個檔有改動/.test($("#tInfo").textContent), $("#tInfo").textContent);
    base.value = "session";
    base.dispatchEvent(new Event("change"));
    await sleep(600);
    check("切到「單一 session」：出現 session 選單", !$("#tBaseSess").hidden && $("#tBaseSess").options.length > 0 && /^session \w{8} · \d+ 個檔有改動/.test($("#tInfo").textContent), $("#tInfo").textContent);
    base.value = "commit";
    base.dispatchEvent(new Event("change"));
    const hash = $("#tBaseHash");
    hash.value = "zz";
    hash.dispatchEvent(new Event("change"));
    await sleep(300);
    const bad = hash.classList.contains("bad");
    hash.value = "abc1234";
    hash.dispatchEvent(new Event("change"));
    await sleep(600);
    check("相對 commit：hash 格式不對標紅、對的就抓", bad && /^相對 abc1234/.test($("#tInfo").textContent), $("#tInfo").textContent);
    base.value = "main";
    base.dispatchEvent(new Event("change"));
    click($(".tab[data-t=sess]"));
    await sleep(600);
    const sess = $$("#sess button");
    check("Sessions tab：含已結束（API 補的）與模型", sess.some((b) => b.classList.contains("ended")) && sess.some((b) => b.querySelector(".mdl")), `${sess.length} 列`);
    click($(".tab[data-t=tree]"));
    await sleep(200);

    out.push("DONE");
    show();
    report();
    // &keepwin：最後讓「即時」浮動視窗開著、藏起結果，方便看截圖裡的視窗樣子
    if (location.search.includes("keepwin")) {
      $("#selftest").style.display = "none";
      click($("#liveToggle"));
    }
  }
  function report() {
    fetch("/__result", { method: "POST", body: out.join("\n") }).catch(() => {});
  }
  window.addEventListener("DOMContentLoaded", () => setTimeout(() => run().catch((e) => { out.push("ERROR " + e.stack); show(); report(); }), 1500));
})();
