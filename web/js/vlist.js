// 虛擬化的版面計算（不碰 DOM）。top[i] 是第 i 行的頂端位移，top[n] 是總高。
// 行數最多幾千行，每次重算 O(n) 就夠快，不需要更複雜的結構。

export function layout(rows, heightOf) {
  const top = new Float64Array(rows.length + 1);
  for (let i = 0; i < rows.length; i++) top[i + 1] = top[i] + heightOf(rows[i]);
  return top;
}

// 位移 y 所在的行：最大的 i 使 top[i] <= y 且該行高度 > 0；夾在 [0, n-1]
export function indexAt(top, y) {
  const n = top.length - 1;
  if (n <= 0) return 0;
  let lo = 0;
  let hi = n - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (top[mid] <= y) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}
