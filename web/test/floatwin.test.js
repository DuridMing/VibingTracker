import { test } from "node:test";
import assert from "node:assert/strict";
import { clampRect } from "../js/floatwin.js";

const VIEW = { vw: 1600, vh: 1000 };

test("在畫面內的位置不變", () => {
  assert.deepEqual(clampRect({ x: 100, y: 200, w: 500, h: 300 }, VIEW), { x: 100, y: 200, w: 500, h: 300 });
});

test("拖出畫面時夾回來：標題列至少留 48px 可抓，頂端不超出畫面", () => {
  // 往左拖出去：右邊至少留 48px 在畫面內
  assert.equal(clampRect({ x: -900, y: 100, w: 500, h: 300 }, VIEW).x, 48 - 500);
  // 往右拖出去：左邊至少留 48px
  assert.equal(clampRect({ x: 1590, y: 100, w: 500, h: 300 }, VIEW).x, 1600 - 48);
  // 往上：標題列不能跑到畫面上方
  assert.equal(clampRect({ x: 100, y: -50, w: 500, h: 300 }, VIEW).y, 0);
  // 往下：標題列至少留在畫面內
  assert.equal(clampRect({ x: 100, y: 2000, w: 500, h: 300 }, VIEW).y, 1000 - 32);
});

test("尺寸有下限，也不會比畫面大", () => {
  assert.deepEqual(clampRect({ x: 0, y: 0, w: 50, h: 20 }, VIEW), { x: 0, y: 0, w: 280, h: 140 });
  const big = clampRect({ x: 0, y: 0, w: 5000, h: 5000 }, VIEW);
  assert.equal(big.w, 1600);
  assert.equal(big.h, 1000);
});

test("缺值或不是數字（例如讀到壞掉的記憶）時用預設", () => {
  const r = clampRect({ x: "a", y: null, w: undefined, h: NaN }, VIEW, { x: 10, y: 20, w: 520, h: 320 });
  assert.deepEqual(r, { x: 10, y: 20, w: 520, h: 320 });
});
