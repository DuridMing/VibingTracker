# 更新紀錄

版號依 [SemVer](https://semver.org/lang/zh-TW/)。發佈時 cargo-dist 會拿對應版號的段落當作 GitHub Release 的說明。

## 0.1.1（2026-10-20）

### 修正

- Windows 上用 `C:\…` 這種反斜線寫法設 `CODING_LAB_HOME` 時，plugin 的自動下載與背景更新一直說「sha256 不符」而裝不起來（每個 session 開始都重新下載一次）。
- Release 的壓縮檔不再帶前端的單元測試（`web/test/`），裝進 `~/.coding-lab/bin/web/` 的只有網頁本身。

## 0.1.0（2026-10-20）

第一個公開版本。GitHub 上的 0.1.0 Release 已撤下（Windows 用反斜線設資料目錄時裝不起來），請用 0.1.1。

### 功能

- **collector**（`coding-lab`）：單一執行檔，Claude Code 的 hook 送到本機 127.0.0.1:4800，同時 tail transcript；事件存成 JSONL（一個 session 一個檔）加 SQLite 索引，資料在 `~/.coding-lab/`，保留 30 天。
- **plugin**：SessionStart 時自動拉起 collector，第一次使用時從 GitHub Release 下載對應平台的二進位；plugin 更新後自動換新版的二進位。追蹤不花任何 token，也不攔截任何工具。
- **網頁**（127.0.0.1:4801）：
  - 所有 session 合成一條串流，每行標 session 與 agent。
  - 注意佇列：危險指令、連續失敗、等你回答、完成待確認。
  - 檔案樹、改動紀錄與 diff、分支視窗、Agents、KPI 與費用。
  - 單一 session 的分頁與監視器視窗。
  - 結束的 session 與整個專案都可以回放。
- **遠端查看**：用金鑰登入。遠端的 thinking 預設點開才載入；敏感檔的內容對遠端一律遮蔽。
- **常駐服務**：Linux 用 systemd user unit、macOS 用 LaunchAgent、Windows 用工作排程器。
- **多機總覽**（hub）：一個畫面看好幾台機器。

### 已知問題

- 主頁的時間軸回放是實驗功能：一次最多回放 24 小時。範圍大時載入會慢：每一段都從各 session 開頭重放，約 10 萬個事件時每段約半秒，期間新事件進畫面會慢一點（Claude Code 的 hook 不受影響）。之後的版本改成從上一段接著算。
- 一台機器同時只能追蹤一個帳號：hook 的 4800 埠是整台機器共用的，後來的帳號的事件會被先啟動的 collector 丟掉（`coding-lab healthz` 顯示「沒在跑」，`collector.log` 有說明）。Windows 和它的 WSL2 也算同一台，只在其中一邊裝 plugin。
- log 的時間是 epoch 毫秒、`coding-lab key list` 顯示 UTC；hub 的「今天」用第一個連上的瀏覽器的時區，跨夏令時間要重開 hub。
- Windows 上 `CODING_LAB_HOME` 用反斜線（`C:\…`）時，自動下載與背景更新會因為「sha256 不符」失敗；改用正斜線（`C:/…`），或不設、用預設的資料目錄就沒問題。0.1.1 已修正。
- macOS 還沒有在實機上逐項驗證，可能遇到的狀況整理在 [macOS 注意事項](docs/macos-warnings.md)。
