// 路徑工具（contracts §4「路徑正規化與比對」，#757 E-D8、#748 L-11）：專案、檔案樹、Sessions 共用這一份。
// - 一律 `/` 分隔、去掉結尾的 `/`，Windows 的磁碟機代號大寫（VS Code 終端機給 e:\，Claude Code 給 E:\）。
// - Windows 路徑（X:/ 開頭）比對時不分大小寫；其他照字比。macOS 的不分大小寫由 collector 端處理。

export function norm(p) {
  const s = String(p ?? "").replace(/\\/g, "/").replace(/\/+$/, "");
  return /^[a-z]:/.test(s) ? s[0].toUpperCase() + s.slice(1) : s;
}

const isWin = (p) => /^[A-Za-z]:/.test(p);

// 兩個已正規化的路徑是不是同一個
export const same = (a, b) => a === b || (isWin(a) && isWin(b) && a.toLowerCase() === b.toLowerCase());

// p 在 base 底下（兩個都已正規化）時回傳相對的部分（相同是 ""），否則 null
// 檔案樹對每個檔都會呼叫：先走一般的字串比對，只有 Windows 路徑才另外不分大小寫比
export function relUnder(p, base) {
  if (p === base) return "";
  if (p.length > base.length && p[base.length] === "/") {
    if (p.startsWith(base)) return p.slice(base.length + 1);
    if (isWin(p) && isWin(base) && p.slice(0, base.length).toLowerCase() === base.toLowerCase()) return p.slice(base.length + 1);
    return null;
  }
  return same(p, base) ? "" : null;
}

export const under = (p, base) => relUnder(p, base) != null;
