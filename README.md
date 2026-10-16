# VibingTracker（coding-lab）

同時開 8–16 個 Claude Code 時，用一個畫面追蹤整個專案、不漏掉任何一個 session。

> **狀態**：規劃定稿，尚未開始實作（階段 0）。下面的安裝與使用是目標行為，做完前不保證可用。完整規劃在 OpenProject 專案 `vibingtracker`，互動原型見 [Coding Lab / Project Tracker](https://claude.ai/artifact/76E5NRYN747KCFn9tzrgu7)。

## 它做什麼

- **合流串流**：所有 session 的 thinking、文字、Read／Edit／Write、Bash 與輸出、git、subagent 動作排成一條，每行標 `[session id 前 8 碼] [agent]`；可依 session 與類型篩選。
- **注意佇列**：危險指令 > 連續失敗 > 等你回答 > 完成待確認，按「查看」切到該 session、按「已確認」消掉。通知（頁內、瀏覽器、提示音）預設全關。
- **檔案樹**：每個檔案被讀／改／新建／失敗的累積次數、正在碰它的 session、相對 `main` 是否有未合併的改動。
- **改動紀錄與修改情形**：跨 session 的 Edit／Write 清單，右欄顯示帶行號的 unified diff。
- **Agents、KPI、模型與費用**：main 與 subagent 的狀態、各模型的 token 與費用。
- **單一 session 深看**：tab 切換，或 ⧉ 開成可拖到第二螢幕的監視器視窗。

追蹤本身**不花 token**：只被動讀取 hook 與 transcript，不往對話塞任何內容；危險指令只標示，不攔截。

## 運作方式

```text
Claude Code ── http hook（127.0.0.1:4800）──┐
            ── transcript JSONL（tail）─────┤
                                            ▼
                               coding-lab（Rust 單一二進位）
                               合併 → JSONL + SQLite → WebSocket
                                            ▼
                               瀏覽器 http://127.0.0.1:4801
```

順序以 transcript 為準（它本來就是 Claude Code 寫的有序日誌），hook 只負責更新「執行中／完成／失敗」與危險判斷，所以思考永遠排在它導致的動作前面。transcript 讀不懂時自動降級成只顯示工具事件。

## 安裝（目標行為）

```bash
claude plugin install coding-lab
```

第一次開 Claude Code session 時，`SessionStart` hook 會執行 `coding-lab ensure`：沒有二進位就從 GitHub Release 下載對應平台的版本（校驗 sha256，放在 `~/.coding-lab/bin/`），沒在跑就拉起 collector。下載失敗只印出手動安裝方式，不會擋住 Claude Code。

打開 <http://127.0.0.1:4801> 即可。10 分鐘沒有 session 時 collector 自動退出。

支援 Linux、macOS、Windows：除了 `SessionStart`，所有事件都用 http hook，不經過 shell，也不需要 Node。`SessionStart` 執行 plugin 內附的 POSIX sh wrapper（`plugin/bin/coding-lab`），Windows 上由 Claude Code 交給 Git Bash 執行。

### 從原始碼安裝（目前的方式）

還沒有發佈二進位，所以先自己編譯，再從這個 repo 安裝 plugin：

```bash
cargo install --path crates/coding-lab          # 裝到 ~/.cargo/bin（wrapper 會在 PATH 上找到它）
claude plugin marketplace add /path/to/VibingTracker
claude plugin install coding-lab@coding-lab
```

wrapper 依序找 `$CODING_LAB_BIN`、`~/.coding-lab/bin/coding-lab`、PATH 上的 `coding-lab`。都找不到時只在 stderr 印出安裝方式，這個 session 就不追蹤，Claude Code 照常運作。只想試一次、不安裝的話，可以用 `claude --plugin-dir plugin`。

### 已知限制：collector 沒在跑時的 hook 錯誤提示

追蹤只會記錄，不會攔截工具，也不會改變 Claude Code 的行為。collector 沒在跑的時候，Claude Code 照常運作、不會變慢，但 **可能會顯示 hook 錯誤提示**，例如互動模式對話裡的 `PreToolUse hook error`，或 `-p` 模式 stderr 的 `SessionEnd hook [http://127.0.0.1:4800/hook] failed: connect ECONNREFUSED`。Claude Code 會把 http hook 連不上視為不阻擋的錯誤，目前沒有設定能關掉這個提示。

平常不會遇到：新 session 的 `SessionStart` 會自動拉起 collector，collector 也要等**所有 session 都結束**、再閒置 10 分鐘才會自己退出。會看到提示的情況：

- collector 當掉，或被手動停掉：開一個新 session 就會重新拉起（`ensure`）。也可以直接執行 `coding-lab serve`。
- session 沒有送出 `SessionEnd` 就消失（例如終端機被直接關掉），而且之後 24 小時都沒有任何 hook：collector 會當作這些 session 已經不在了而退出。
- 找不到 collector 二進位：`SessionStart` 只會在 stderr 印出安裝方式，這個 session 就不追蹤。

不想看到提示時，可以停用 plugin：`claude plugin disable coding-lab`。

## 設定

`~/.coding-lab/config.toml`（優先序：命令列 > `CODING_LAB_*` 環境變數 > 設定檔 > 預設）：

```toml
[hook]
bind = "127.0.0.1:4800"   # 只給同機的 Claude Code，永遠不開外

[web]
bind = "127.0.0.1:4801"   # 要從別台看就改 tailnet IP，例如 "100.x.x.x:4801"
auth = "local-open"       # 多人共用的機器改成 "always"，連 localhost 也要金鑰
remote_thinking = "on_demand"   # 遠端看 thinking：on_demand（點開才載入）| hide | show
```

| 檔案 | 用途 |
| --- | --- |
| `~/.coding-lab/rules.json` | 覆寫危險指令規則（正則、原因、等級 `warn`／`danger`） |
| `~/.coding-lab/prices.json` | 覆寫模型單價表 |
| `~/.coding-lab/collector.log` | 日誌，10 MB × 5 輪替 |

資料保留 30 天（`coding-lab prune`）。

### 從本機看伺服器上的 session

在伺服器上開發、從自己的電腦看是主要用法，有兩種連法：

- **SSH 通道**（最簡單）：`ssh -N -L 4801:127.0.0.1:4801 <server>`，然後在本機開 <http://127.0.0.1:4801>。
- **tailnet**：伺服器的 `[web].bind` 改成 tailnet IP，用金鑰登入。在伺服器上執行 `coding-lab key create --name laptop` 產生金鑰（只會顯示一次），或用 `coding-lab key login-link` 印出 5 分鐘有效的一次性登入連結。

要一次看多台伺服器，在本機跑 `coding-lab hub`，用 `coding-lab hub add <名稱> <網址>` 加入各台。

遠端的 thinking 預設收合、點開才載入；讀 `.env` 這類敏感輸出在遠端一律不顯示。金鑰驗證與 hub 仍在規劃中。

## 開發

需要 Rust stable（`cargo`）。

```bash
scripts/check.sh                     # fmt、clippy、test；commit 前必跑
cargo run -p coding-lab -- serve     # 前景跑 collector
```

開發時設 `CODING_LAB_HOME` 指到暫存目錄，避免動到正在用的 `~/.coding-lab/`。

### 分支（git-flow）

| 分支 | 用途 |
| --- | --- |
| `main` | 已發布的版本，每個合併都打 tag `vX.Y.Z`；不直接 commit |
| `develop` | 日常開發的整合分支 |
| `feature/<名稱>` | 從 `develop` 開，完成後 `--no-ff` 併回 `develop` |
| `release/X.Y.Z` | 從 `develop` 開，只改版號與修 bug；併入 `main` 打 tag 後再併回 `develop` |
| `hotfix/X.Y.Z` | 從 `main` 開，修好後併入 `main` 打 tag，再併回 `develop` |

Commit message 用繁體中文並加 `refs #<OpenProject 工作項目 id>`。

### 進度

工作項目在 OpenProject 專案 `vibingtracker`：每個階段一個 Version（P0–P5）。

| 階段 | 內容 |
| --- | --- |
| P0 看清資料 | 收真實 hook／transcript 樣本、回答未決問題、定資料契約、repo 骨架 |
| P1 管線 | plugin、collector 六模組、JSONL＋SQLite、WS 頻道、量效能基準 |
| P2 合流串流 | 三欄畫面用真資料跑起來：串流、改動紀錄、diff、Agents、檔案樹、Sessions、KPI |
| P3 狀態與注意佇列 | `session.status`、佇列、通知開關 |
| P4 單 session 與監視器 | session tab、監視器視窗、git 列、resume／compact |
| P5 韌性與體驗 | 降級、回放、隱私、清理、三平台發佈、乾淨機器驗收 |

## 文件

使用手冊等給使用者的文件放在 `docs/`。規劃（需求、架構、資料模型、決策與風險）與協作規範不進 git（見 `.gitignore`），同步在 OpenProject 專案 `vibingtracker` 的「文件」。
