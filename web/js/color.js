// session 固定色：由完整 id 雜湊決定，不依出現順序。
// 主視窗、監視器視窗、重新整理之後都同色，不需要互相同步。
// 色票定義在 css/app.css 的 --s0 … --s11。

export const PALETTE_SIZE = 12;

// FNV-1a 32-bit
function hash(s) {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

export function sessionColorIndex(id) {
  return hash(String(id)) % PALETTE_SIZE;
}

export function sessionColor(id) {
  return `var(--s${sessionColorIndex(id)})`;
}

export function shortId(id) {
  return String(id).slice(0, 8);
}
