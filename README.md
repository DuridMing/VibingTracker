# VibingTracker（coding-lab）

同時開 8–16 個 Claude Code 時，用一個畫面追蹤整個專案、不漏掉任何一個 session。

> **狀態**：collector、plugin 與網頁都可以用，遠端登入（金鑰）、遠端 thinking、多機總覽（hub）與常駐服務也已完成並在 Linux 與 Windows 實機驗收過。正式版 0.1.0 發佈後，plugin 會自動下載對應平台的二進位；在那之前要從原始碼安裝。主頁的時間軸回放是實驗功能，大範圍回放會慢。

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

## 安裝

一步一步的安裝、更新、疑難排解與卸載見 [安裝手冊](docs/install.md)（也可以交給 Claude Code 照[用 AI 安裝](docs/install-with-ai.md)裝）；裝好之後網頁怎麼用見 [使用手冊](docs/usage.md)，macOS 另外見 [macOS 注意事項](docs/macos-warnings.md)。下面是摘要。

需要 Rust stable（`cargo`）與 Claude Code。支援 Linux、macOS、Windows：除了 `SessionStart`，所有事件都用 http hook，不經過 shell，也不需要 Node。`SessionStart` 執行 plugin 內附的 POSIX sh wrapper（`plugin/bin/coding-lab`），Windows 上由 Claude Code 交給 Git Bash 執行，所以 Windows 要先裝 [Git for Windows](https://git-scm.com/download/win)。

Claude Code CLI 的安裝（`plugin` 指令要用到）：

```bash
curl -fsSL https://claude.ai/install.sh | bash        # Linux、macOS
irm https://claude.ai/install.ps1 | iex               # Windows PowerShell
```

### 1. 編譯並放好二進位與網頁

二進位和 `web/` 要放在同一個資料夾：collector 沒有另外設定網頁位置時，會用二進位旁邊的 `web/`。Windows 在 Git Bash 執行下面的指令（`~` 是 `%USERPROFILE%`）。

```bash
git clone -b develop https://github.com/DuridMing/VibingTracker.git && cd VibingTracker
cargo build --release -p coding-lab
mkdir -p ~/.coding-lab/bin
cp target/release/coding-lab ~/.coding-lab/bin/     # Windows：target/release/coding-lab.exe
cp -r web ~/.coding-lab/bin/
```

也可以用 `cargo install --path crates/coding-lab` 裝到 `~/.cargo/bin`，但那裡沒有 `web/`，要在設定檔另外指定網頁位置（見「設定」的 `[web].root`），否則網頁打不開。

### 2. 安裝 plugin

```bash
claude plugin marketplace add /path/to/VibingTracker
claude plugin install coding-lab@coding-lab
```

`/path/to/VibingTracker` 是上一步 clone 下來的資料夾（Windows 例如 `C:\src\VibingTracker`）。目前程式碼在 `develop` 分支，`main` 還沒有發佈的版本，所以 marketplace 要用本機的 clone，不要直接給 GitHub 的 `DuridMing/VibingTracker`（會讀到 `main`）。也可以在 Claude Code 裡輸入 `/plugin` 用選單安裝。

plugin 裝在使用者層級（`~/.claude/`），Claude Desktop 的 Code 分頁開的本機 session 也會載入，一樣會被追蹤（Windows 上實測過）。

只想試一次、不安裝的話，可以用 `claude --plugin-dir /path/to/VibingTracker/plugin`。

### 3. 開一個 Claude Code session

`SessionStart` hook 會執行 `coding-lab ensure`：collector 沒在跑就拉起它，並把這個 session 交給它追蹤。打開 <http://127.0.0.1:4801> 即可。所有 session 都結束、再閒置 10 分鐘後，collector 會自己退出，下一個 session 開始時再拉起。

確認 collector 在跑：

```bash
~/.coding-lab/bin/coding-lab healthz
```

wrapper 依序找 `$CODING_LAB_BIN`、`~/.coding-lab/bin/coding-lab`、PATH 上的 `coding-lab`。都找不到時只在 stderr 印出安裝方式，這個 session 就不追蹤，Claude Code 照常運作。

### 從 Release 自動下載

wrapper 找不到二進位時，會在背景從 GitHub Release 下載**和 plugin 同版**的壓縮檔（cargo-dist 產生的 `coding-lab-<平台>.tar.gz`／`.zip` 與 `.sha256`），不必編譯。要改下載來源（自己架的鏡像）或關掉自動下載，設在 Claude Code 讀得到的環境變數：

```bash
export CODING_LAB_RELEASE_URL=https://example.com/coding-lab/releases/v0.1.0   # 放下載檔的目錄網址；只收 https://
export CODING_LAB_RELEASE_URL=                                                 # 空字串：不自動下載
```

sha256 相符才連同 `web/` 解到 `~/.coding-lab/bin/`。這個 session 不追蹤，下一個 session 開始；過程記在 `~/.coding-lab/install.log`。下載或校驗失敗時只印出手動安裝方式，不會擋住 Claude Code。要在前景下載、直接看到結果：

```bash
/path/to/VibingTracker/plugin/bin/coding-lab install    # 印出 platform 可看這台機器對應哪個檔
```

plugin 更新之後，wrapper 自己裝的二進位比 plugin 舊時，會照用舊的、同時在背景下載新版，下一個 session 開始用新版（手動放的二進位不會被換掉）。

支援的平台：Linux x86_64／aarch64、macOS x86_64／Apple Silicon、Windows x86_64。

### 已知限制：collector 沒在跑時的 hook 錯誤提示

追蹤只會記錄，不會攔截工具，也不會改變 Claude Code 的行為。collector 沒在跑的時候，Claude Code 照常運作，但 **可能會顯示 hook 錯誤提示**。Linux 與 macOS 上連不上會立刻失敗、不會變慢；Windows 連沒在聽的埠要約 2 秒才回拒絕，所以每個 hook 會多等到逾時（1 秒），例如互動模式對話裡的 `PreToolUse hook error`，或 `-p` 模式 stderr 的 `SessionEnd hook [http://127.0.0.1:4800/hook] failed: connect ECONNREFUSED`。Claude Code 會把 http hook 連不上視為不阻擋的錯誤，目前沒有設定能關掉這個提示。

平常不會遇到：新 session 的 `SessionStart` 會自動拉起 collector，collector 也要等**所有 session 都結束**、再閒置 10 分鐘才會自己退出。會看到提示的情況：

- collector 當掉，或被手動停掉（`coding-lab stop`）：開一個新 session 就會重新拉起（`ensure`）。也可以直接執行 `coding-lab serve`（前景執行，關掉終端機就停）。
- session 沒有送出 `SessionEnd` 就消失（例如終端機被直接關掉），而且之後 24 小時都沒有任何 hook：collector 會當作這些 session 已經不在了而退出。
- 找不到 collector 二進位：`SessionStart` 只會在 stderr 印出安裝方式，這個 session 就不追蹤。

不想看到提示時，可以停用 plugin：`claude plugin disable coding-lab`。

Windows 上另外兩種情況也會看到這些提示：`SessionStart` 超過 5 秒（Git Bash 冷啟動、Defender 掃描新執行檔），以及 4800／4801 被 Hyper-V／WSL 劃進保留埠範圍。處理方式見[安裝手冊的疑難排解](docs/install.md#疑難排解)。

## 設定

`~/.coding-lab/config.toml`（優先序：命令列 > `CODING_LAB_*` 環境變數 > 設定檔 > 預設）。`coding-lab config` 印出合併後實際生效的設定。

```toml
[hook]
bind = "127.0.0.1:4800"   # 只給同機的 Claude Code，只能是 127.0.0.1；plugin 的 hook 固定送到 4800，改埠只適合測試

[web]
bind = "127.0.0.1:4801"   # 從別台看：改成 tailnet IP（100.x.x.x）並用金鑰登入，或維持 127.0.0.1 走 SSH 通道（見下方）
# bind = ["127.0.0.1:4801", "100.x.x.x:4801"]   # 兩個都要：127.0.0.1 給 SSH 通道（免金鑰）、tailnet 給直連（要金鑰）
root = "/path/to/VibingTracker/web"   # 網頁位置；不設時用二進位旁邊的 web/
auth = "local-open"       # local-open：127.0.0.1 連進來免金鑰（含 SSH 通道）；always：連本機也要金鑰（多人共用的伺服器）
remote_thinking = "on_demand"   # 遠端連線的 thinking：on_demand 點開才載入（預設）| hide | show；本機永遠完整
# allow_insecure = true   # bind 是公開 IP 又沒有 TLS 時才需要（明文 http 會被竊聽；建議前面放 Caddy）

[collector]
idle_exit_seconds = 600   # 所有 session 都結束後閒置多久自動退出；0＝不退出
wait_guess_seconds = 30   # Claude 最後寫了一段話、之後這麼久沒有任何動作時，注意佇列推測「等你回答」；0＝關閉
```

| 環境變數 | 對應 |
| --- | --- |
| `CODING_LAB_HOME` | 資料目錄（預設 `~/.coding-lab`） |
| `CODING_LAB_HOOK_BIND`、`CODING_LAB_WEB_BIND`、`CODING_LAB_WEB_ROOT`、`CODING_LAB_IDLE_EXIT_SECONDS`、`CODING_LAB_WAIT_GUESS_SECONDS`、`CODING_LAB_WEB_AUTH`、`CODING_LAB_REMOTE_THINKING`、`CODING_LAB_ALLOW_INSECURE` | 同上表的設定 |
| `CODING_LAB_BIN` | wrapper 要用的二進位（優先於 `~/.coding-lab/bin`） |
| `CODING_LAB_RELEASE_URL` | wrapper 自動下載的來源（見「從 Release 自動下載」） |

| 檔案 | 用途 |
| --- | --- |
| `~/.coding-lab/rules.json` | 覆寫危險指令規則（格式見下方） |
| `~/.coding-lab/prices.json` | 覆寫模型單價表 |
| `~/.coding-lab/keys.json` | 遠端查看用的金鑰（只存雜湊，權限必須是 600）；用 `coding-lab key` 管理，不要手改 |
| `~/.coding-lab/collector.log` | 日誌，10 MB × 5 輪替（`collector.log.1`…`.5`）；前景執行 `coding-lab serve` 時改印在終端機 |

`prices.json` 只要列出想改的模型，單位是 USD／每百萬 tokens，改完重啟 collector 才生效；格式有錯時整個改用內建單價，並在日誌寫一行警告：

```json
{ "claude-opus-5-5": { "input": 4.0, "output": 20.0, "cacheRead": 0.2 } }
```

`rules.json` 和內建規則同格式：同 `id` 的取代內建、新的 `id` 優先比對、`disable` 列出要停用的內建規則。`pattern` 是正則。指令型工具（Bash、PowerShell；`tools` 寫 `"@command"` 代表全部，`cmd /c …` 裡的指令也算）比對的是**分詞後**依 `&&`、`;`、`|`、換行切開的每一段：引號裡的符號不算分隔、反斜線續行接回同一段、heredoc 的本文當成字面文字不當指令、`$(…)` 與 `bash -c`、`cmd /c` 的內容另外檢查，`sudo`、`env`、`timeout` 等包裝先去掉（`"scope": "command"` 改成比對接回來的整條指令）。其他工具預設比對 `file_path`（可用 `field` 指定）。選填的 `unless` 也是正則，`pattern` 命中而 `unless` 也命中時不算，例如排除 `--dry-run`。改完重啟 collector 才生效；格式或正則有錯時整份不用、退回內建規則，並在日誌寫一行警告。危險只標示，不會攔截。

```json
{
  "rules": [
    { "id": "kubectl-delete", "tools": ["@command"], "pattern": "^kubectl\\s+delete\\b", "unless": "\\s--dry-run(=|\\s|$)", "level": "danger", "why": "刪除叢集資源" }
  ],
  "disable": ["rm-rf-build"]
}
```

資料保留 30 天：最後活動超過 30 天的 session 會連同事件檔一起刪掉。collector 啟動時和之後每天會自動清理；collector 沒在跑時可以用 `coding-lab prune` 手動清（collector 在跑時這個指令不會動資料）。

### 從本機看伺服器上的 session

在伺服器上開發、從自己的電腦看，有兩種連法。

**SSH 通道**（不用改設定）：

```bash
ssh -N -L 4801:127.0.0.1:4801 <server>
```

然後在本機開 <http://127.0.0.1:4801>。網頁只綁在伺服器的 127.0.0.1，不需要防火牆也不需要金鑰（通道進來的連線就是本機）。

**tailnet 直連**（Tailscale／ZeroTier；手機也能看）：在伺服器上把 `[web].bind` 改成它的 tailnet IP（要同時保留 SSH 通道就寫成陣列 `["127.0.0.1:4801", "100.x.x.x:4801"]`），再建一把金鑰：

```bash
coding-lab key create --name laptop --scope view --expires 90d   # 明文只印這一次
coding-lab stop                                                   # 讓下一個 session 用新設定重新拉起 collector
```

本機開 `http://100.x.x.x:4801`，第一次會出現登入頁，貼上金鑰；之後 30 天不必再貼。不想在手機上打一長串，可以在伺服器終端機上執行 `coding-lab key login-link`，它印出一個 5 分鐘內只能用一次的網址，點開就登入。

遠端連線一律要金鑰，而且看不到命中敏感規則的工具輸出（例如讀 `.env`）；thinking 預設點開才載入（`remote_thinking`）。`view` 金鑰只能看，`operate` 才能按注意佇列的「已確認」。撤銷：`coding-lab key revoke <id>`，持有它的瀏覽器下一個請求起就要重新登入。金鑰與 cookie 走明文 http，所以只建議 tailnet（WireGuard 加密）、SSH 通道或自己前置 TLS（Caddy）；綁公開 IP 時 collector 會拒絕啟動，除非設 `allow_insecure = true`。

多人共用的伺服器上，其他使用者也連得到 127.0.0.1，這時設 `auth = "always"`，連本機（含 SSH 通道）都要金鑰。

**同一台機器只能有一個帳號使用 coding-lab**：hook 入口 4800 與網頁 4801 是整台機器共用的，Claude Code 的 hook 網址也不能依帳號變化。最先開 session 的帳號拿到 collector；其他帳號的 hook 會帶上自己的使用者名稱，collector 認得出不是自己的就丟掉（不記錄、不顯示，collector.log 記一行），所以那些帳號不會被追蹤。`coding-lab healthz` 會印出 collector 是哪個帳號的。多人共用的伺服器建議只在一個帳號裝 plugin，並且每個帳號都設 `auth = "always"`。

**Windows 與 WSL2 也算同一台機器**：WSL2 預設的 localhost 轉送（以及 mirrored 網路模式）會讓兩邊共用 4800，先起來的那邊收到兩邊的 hook，另一邊不會被追蹤，而且讀不到對方檔案系統上的 transcript 與 git。同一台機器的 Windows 與 WSL 請只在其中一邊裝 plugin。

**讓伺服器重開機後 collector 自己起來**：平常 collector 由第一個 session 的 `ensure` 拉起、閒置 10 分鐘退出；伺服器上想隨時都看得到（含沒有 session 在跑的時候），裝成常駐服務：

```bash
coding-lab service install      # Linux：systemd user unit；macOS：LaunchAgent；Windows：工作排程器（登入時啟動）
coding-lab service status
coding-lab service uninstall
```

服務用 `serve --idle-exit-seconds 0` 跑、不會閒置結束，設定仍讀 `~/.coding-lab/config.toml`。Linux 要在沒人登入時就起來，再執行一次 `loginctl enable-linger $USER`。`install --dry-run` 只印出會寫的檔與會跑的指令。TLS 不內建：需要 https 時前置 Caddy 或 nginx 反向代理到 127.0.0.1:4801，並設 `[web].auth = "always"`（反代轉來的連線一律當遠端、要金鑰；細節見安裝手冊）。

**一次看多台伺服器（hub）**：每台伺服器各跑自己的 collector（上面的形態），本機跑 `coding-lab hub` 把它們合成一頁：

```bash
coding-lab hub add build-01 http://100.64.0.5:4801      # 會問金鑰（在那台 key create 的 clk_…），不進 shell 歷史
coding-lab hub add tunnel http://127.0.0.1:14801 --no-key   # SSH 通道（ssh -N -L 14801:127.0.0.1:4801 <伺服器>）的不用金鑰
coding-lab hub                                           # 印出 http://127.0.0.1:4802/?token=…，用這個網址開
```

127.0.0.1 是整台機器共用的，hub 又拿著上游的金鑰，所以 hub 每次啟動都產生新的 token、只印在執行它的終端機：用印出的網址開，第一次會換成 cookie（HttpOnly、SameSite=Strict），之後網址列不再帶 token；沒有 token 也沒有 cookie 的 `/api`、`/ws` 一律 401。hub 重新啟動後舊的 cookie 失效，要用新印出的網址。腳本可以用 `Authorization: Bearer <token>`。

串流每行前面多一個 `[主機]`，Sessions、Agents 也標主機；某台斷線時它的 session 標「離線」、hub 自動重連，其他台照常。hub 只轉發不存資料；要「載入更多」、看 thinking 全文、按「已確認」都轉給對應的那台（那台的金鑰是 `view` 就不能確認）。上游列在 `~/.coding-lab/config.toml` 的 `[[hub.upstream]]`（`name`、`url`、`key`；檔案權限 600），hub 的埠用 `[hub] bind` 或 `--bind` 改（只能是本機位址）。上游網址只接受 `http://`：遠端一律走 tailnet 或 SSH 通道。

## 子命令

| 指令 | 用途 |
| --- | --- |
| `coding-lab serve` | 前景執行 collector（hook 入口 4800、網頁 4801）；平常由 `ensure` 在背景拉起，不必手動執行 |
| `coding-lab ensure` | 沒在跑就拉起 collector，並轉送 stdin 的 SessionStart；由 plugin 的 `SessionStart` hook 呼叫，失敗也 exit 0 |
| `coding-lab healthz` | 查詢執行中 collector 的版本、session 數（`open` 是還開著的，`sessions` 含 2 小時內結束的）與事件迴圈延遲 |
| `coding-lab config` | 印出合併後的設定 |
| `coding-lab prune` | 立刻清掉 30 天前的資料（collector 在跑時不動資料，它自己每天會清） |
| `coding-lab stop` | 請背景的 collector 正常結束（寫完紀錄才回來） |
| `coding-lab key create --name <名稱> [--scope view\|operate] [--expires 90d\|12h\|30m\|never]` | 建一把遠端查看用的金鑰，明文只印這一次；`view` 只能看，`operate` 另外可按「已確認」、改通知設定 |
| `coding-lab key list`、`coding-lab key revoke <id>` | 列出金鑰（不含 secret）、撤銷（立即失效，不必重啟 collector） |
| `coding-lab key login-link [--key <id>]` | 印一個 5 分鐘內只能用一次的登入網址（金鑰不經過網址列與瀏覽紀錄） |
| `coding-lab service install [--dry-run]`、`service uninstall`、`service status` | 裝成開機（登入）自動啟動、不閒置結束的常駐服務：Linux systemd user unit、macOS LaunchAgent、Windows 工作排程器；`uninstall` 只刪 `install` 寫的檔 |
| `coding-lab hub [--bind 127.0.0.1:4802]`、`hub add <名稱> <http://主機:4801> [--no-key]` | 多機總覽：對 `[[hub.upstream]]` 的每台開 WS 合流，在本機開 web；`add` 互動輸入金鑰並寫進 config.toml |

## 卸載

```bash
claude plugin uninstall coding-lab@coding-lab
claude plugin marketplace remove coding-lab
```

然後請 collector 結束（寫完紀錄才回來；沒在跑也不會報錯），再刪掉二進位與資料：

```bash
coding-lab stop
rm -rf ~/.coding-lab      # 二進位、網頁、設定、所有追蹤紀錄（JSONL 與 SQLite）
```

用 `cargo install` 裝的話，另外執行 `cargo uninstall coding-lab`。

## 開發

需要 Rust stable（`cargo`）；前端測試用 podman 跑 `node:22.23.3-alpine`，主機不需要 Node。

```bash
scripts/check.sh                     # fmt、clippy、test、前端測試；commit 前必跑
cargo run -p coding-lab -- serve --web-root web   # 前景跑 collector
python3 -m http.server 4802 -d web   # 只看前端：開 127.0.0.1:4802/?mock=12（假資料，不需要 collector）
```

開發時設 `CODING_LAB_HOME` 指到暫存目錄，避免動到正在用的 `~/.coding-lab/`。

Windows 開發機：指令都在 **Git Bash** 跑（`scripts/`、`tests/scripts/` 的腳本是 bash，PowerShell 不行）；前端測試要有 podman（或在 WSL 裡跑），沒有的話至少跑 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`。

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

工作項目在 OpenProject 專案 `vibingtracker`：每個階段一個 Version（P0–P6）。

| 階段 | 內容 |
| --- | --- |
| P0 看清資料 | 收真實 hook／transcript 樣本、回答未決問題、定資料契約、repo 骨架 |
| P1 管線 | plugin、collector 六模組、JSONL＋SQLite、WS 頻道、量效能基準 |
| P2 合流串流 | 三欄畫面用真資料跑起來：串流、改動紀錄、diff、Agents、檔案樹、Sessions、KPI |
| P3 狀態與注意佇列 | `session.status`、佇列、通知開關 |
| P4 單 session 與監視器 | session tab、監視器視窗、git 列、resume／compact |
| P5 韌性與體驗 | 降級、回放、隱私、清理、三平台發佈、乾淨機器驗收 |
| P6 遠端與全域 | 金鑰驗證、遠端 thinking、常駐服務、全部專案切換、多機總覽（hub） |

## 文件

使用手冊等給使用者的文件放在 `docs/`。規劃（需求、架構、資料模型、決策與風險）與協作規範不進 git（見 `.gitignore`），同步在 OpenProject 專案 `vibingtracker` 的「文件」。
