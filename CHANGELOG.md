# 更新紀錄

版號依 [SemVer](https://semver.org/lang/zh-TW/)。發佈時 cargo-dist 會拿對應版號的段落當作 GitHub Release 的說明。

## 0.1.0（尚未發佈）

第一個公開版本。

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

- 主頁的時間軸回放是實驗功能：一次最多回放 24 小時，範圍大時載入會慢。
