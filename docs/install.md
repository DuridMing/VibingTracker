# 安裝手冊

這份手冊帶你從零裝好 coding-lab：編譯 collector、裝 Claude Code plugin、確認追蹤有在跑。目前還沒有發佈好的二進位，要從原始碼編譯，大約 10 分鐘（大部分是第一次編譯）。

裝好之後，每次開 Claude Code session 都會自動被追蹤，打開 <http://127.0.0.1:4801> 就能看到所有 session 合在一起的串流。追蹤不花 token，也不會攔截或改變 Claude Code 的行為。

## 目錄

1. [需要的東西](#需要的東西)
2. [編譯並放好 collector](#編譯並放好-collector)
3. [安裝 plugin](#安裝-plugin)
4. [確認有在追蹤](#確認有在追蹤)
5. [讓 collector 常駐（伺服器重開機後自己起來）](#讓-collector-常駐伺服器重開機後自己起來)
6. [一次看多台伺服器（hub）](#一次看多台伺服器hub)
7. [同一台機器上有好幾個專案](#同一台機器上有好幾個專案)
8. [看過去的 session（回放）](#看過去的-session回放)
9. [更新](#更新)
10. [疑難排解](#疑難排解)
11. [卸載](#卸載)

## 需要的東西

| 項目 | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Git | 系統套件 | Xcode Command Line Tools（`xcode-select --install`） | [Git for Windows](https://git-scm.com/download/win)（**必裝**：plugin 的啟動腳本由 Git Bash 執行） |
| Rust | [rustup](https://rustup.rs) | [rustup](https://rustup.rs) | [rustup](https://rustup.rs)，安裝時選預設的 MSVC；另外要裝 Visual Studio Build Tools 的「使用 C++ 的桌面開發」 |
| Claude Code CLI | `curl -fsSL https://claude.ai/install.sh \| bash` | 同左 | PowerShell：`irm https://claude.ai/install.ps1 \| iex` |

裝完開一個新的終端機，確認三個指令都找得到：

```bash
git --version
cargo --version
claude --version
```

Claude Desktop 不需要另外處理：plugin 用 CLI 裝好之後，Desktop 的 Code 分頁開的本機 session 也會被追蹤。不過**安裝 plugin 本身要用 CLI**，所以 Windows 上只裝了 Desktop 的話，還是要裝 CLI。

## 編譯並放好 collector

collector 是一個執行檔（`coding-lab`），網頁檔放在它旁邊的 `web/` 資料夾。兩者要放在同一個地方：`~/.coding-lab/bin/`（Windows 是 `%USERPROFILE%\.coding-lab\bin\`）。plugin 會到這裡找執行檔。

**Linux、macOS**（終端機）：

```bash
git clone -b develop https://github.com/DuridMing/VibingTracker.git
cd VibingTracker
cargo build --release -p coding-lab
mkdir -p ~/.coding-lab/bin
cp target/release/coding-lab ~/.coding-lab/bin/
cp -r web ~/.coding-lab/bin/
```

**Windows**（開 **Git Bash**，不是 PowerShell）：

```bash
git clone -b develop https://github.com/DuridMing/VibingTracker.git
cd VibingTracker
cargo build --release -p coding-lab
mkdir -p ~/.coding-lab/bin
cp target/release/coding-lab.exe ~/.coding-lab/bin/
cp -r web ~/.coding-lab/bin/
```

注意：

- 程式碼目前在 `develop` 分支，`main` 還沒有發佈的版本，所以 clone 要加 `-b develop`。
- clone 下來的資料夾不要刪，下一步安裝 plugin 會用到它。
- 確認放好了：`~/.coding-lab/bin/` 底下要有 `coding-lab`（或 `coding-lab.exe`）和 `web/` 資料夾。

## 安裝 plugin

把剛剛 clone 下來的資料夾加成 marketplace，再從裡面裝 plugin。下面的路徑換成你的實際位置：

```bash
claude plugin marketplace add /path/to/VibingTracker
claude plugin install coding-lab@coding-lab
```

Windows 在 PowerShell 或 Git Bash 都可以，路徑例如 `C:\src\VibingTracker`。

- 一定要用**本機的資料夾**。直接給 GitHub 的 `DuridMing/VibingTracker` 會讀到 `main` 分支，那裡沒有 plugin。
- 也可以在 Claude Code 裡輸入 `/plugin`，用選單加 marketplace 與安裝。
- 只想試一次、不安裝：`claude --plugin-dir /path/to/VibingTracker/plugin`。

## 確認有在追蹤

1. **開一個新的 Claude Code session**。CLI 或 Desktop 的 Code 分頁都可以。裝 plugin 之前就開著的 session：Desktop 會從裝好那一刻開始追蹤（之前的工具呼叫不會補）；CLI 要重開一個。剛裝好的那一刻，已開著的 session 會開始送事件、但沒有人把 collector 拉起來；網頁連不上就重開一個 session，或在終端機跑一次 `~/.coding-lab/bin/coding-lab ensure`（Windows 在 Git Bash 跑 `sh <plugin 目錄>/bin/coding-lab ensure`）。
2. 請 Claude 做一件會用到工具的事，例如「讀 README 的第一行」。
3. 瀏覽器打開 <http://127.0.0.1:4801>，應該會看到這個 session 和它剛剛做的事。

也可以在終端機確認 collector 在跑：

```bash
~/.coding-lab/bin/coding-lab healthz
```

會印出版本、`sessions`（正在追蹤幾個 session）等資訊。Windows 在 Git Bash 用同一行，或在 PowerShell 用 `& "$env:USERPROFILE\.coding-lab\bin\coding-lab.exe" healthz`。

collector 不用自己啟動：每個 session 開始時，plugin 會在它沒在跑時把它拉起來；所有 session 都結束、再閒置 10 分鐘後它會自己退出。

在另一台電腦上看（例如程式跑在伺服器、瀏覽器在筆電），用 SSH 通道把網頁接過來：

```bash
ssh -N -L 4801:127.0.0.1:4801 <伺服器>
```

再在本機開 <http://127.0.0.1:4801>。用 VS Code Remote 連線的話，也可以在「連接埠（Ports）」面板轉送 4801。

不想每次開通道、或想用手機看，可以改走 Tailscale／ZeroTier 直連：

1. 在伺服器的 `~/.coding-lab/config.toml` 設 `[web] bind = "100.x.x.x:4801"`（這台的 tailnet IP）。SSH 通道也要繼續用的話寫成陣列：`bind = ["127.0.0.1:4801", "100.x.x.x:4801"]`，通道進來的仍免金鑰、直連的要金鑰。
2. 建一把金鑰：`~/.coding-lab/bin/coding-lab key create --name laptop --scope view --expires 90d`。印出來的 `clk_…` 只會出現這一次，先複製。
3. `~/.coding-lab/bin/coding-lab stop`，下一個 session 開始時 collector 會用新設定重新拉起。
4. 本機或手機開 `http://100.x.x.x:4801`，在登入頁貼上金鑰。之後 30 天不必再貼。手機不好打字的話，在伺服器終端機執行 `coding-lab key login-link`，用手機掃或打開它印出的網址（5 分鐘內、只能用一次）。

遠端看不到命中敏感規則的工具輸出，thinking 預設點開才載入。撤銷金鑰：`coding-lab key revoke <id>`（`key list` 看 id）。綁公開 IP 會被拒絕啟動，除非自己前置 TLS 並設 `allow_insecure = true`——明文 http 上的金鑰會被同網段竊聽。

## 讓 collector 常駐（伺服器重開機後自己起來）

平常 collector 由每個 session 的 SessionStart 拉起，沒有 session 再閒置 10 分鐘就退出；要從別台電腦隨時看、或重開機後不必先開一個 session，就把它裝成使用者層級的常駐服務（不需要 root）：

```bash
~/.coding-lab/bin/coding-lab service install
```

| 平台 | 寫哪裡 | 怎麼啟動 |
| --- | --- | --- |
| Linux | `~/.config/systemd/user/coding-lab.service` | `systemctl --user enable --now`；沒人登入也要跑的話再執行一次 `loginctl enable-linger $USER` |
| macOS | `~/Library/LaunchAgents/io.coding-lab.collector.plist` | `launchctl bootstrap gui/<uid>`；第一次會跳「允許在背景執行」，允許即可 |
| Windows | `%USERPROFILE%\.coding-lab\coding-lab-task.xml` | 工作排程器的 `CodingLab` 工作，登入時啟動、隱藏視窗（要在沒人登入時跑，在工作排程器裡改成「不論使用者登入與否均執行」） |

安裝時會先請正在跑的 collector 結束（服務要接手同一個埠），服務用 `serve --home ~/.coding-lab --idle-exit-seconds 0` 跑，設定一樣讀 `~/.coding-lab/config.toml`（改了 `[web].bind` 等設定後重啟服務：Linux `systemctl --user restart coding-lab`、macOS `launchctl kickstart -k gui/$(id -u)/io.coding-lab.collector`、Windows `schtasks /End /TN CodingLab` 再 `schtasks /Run /TN CodingLab`）。plugin 的 `ensure` 看到埠有人就不會再拉第二個。

- `service status`：有沒有裝、collector 現在有沒有在跑。
- `service install --dry-run`：只印會寫的檔與會跑的指令。
- `service uninstall`：停掉、取消註冊、刪掉上面那個檔；之後又回到由 session 拉起的模式。
- 更新執行檔時先 `service uninstall` 或停掉服務再覆蓋，覆蓋完再 `service install`（Windows 不能覆蓋執行中的 .exe）。

https 不內建：需要時前置 Caddy／nginx 反向代理到 127.0.0.1:4801，或走 tailnet、SSH 通道（見上一節）。

## 一次看多台伺服器（hub）

幾台伺服器各自跑 Claude Code 與 collector，想在自己的電腦上一頁看完：每台照上一節設成常駐、給本機一把金鑰（tailnet）或開 SSH 通道，然後在本機：

```bash
~/.coding-lab/bin/coding-lab hub add build-01 http://100.64.0.5:4801   # 問金鑰：貼上在 build-01 上 key create 印出的 clk_…
~/.coding-lab/bin/coding-lab hub add build-02 http://127.0.0.1:14801 --no-key   # 走 SSH 通道的那台不用金鑰
~/.coding-lab/bin/coding-lab hub
```

打開 <http://127.0.0.1:4802>：和單機的頁面一樣，只是每行前面多一個 `[主機名]`，Sessions 與 Agents 也標主機，連線狀態列顯示 `hub`。

- 某台斷線（網路斷、重開機）：它的 session 標「離線」、分頁變淡，其他台照常；hub 每 1～30 秒重連，連回來會整頁重載那台的內容。
- 「載入更多」、thinking 點開、檔案樹、分支視窗、回放、「已確認」都由 hub 轉給對應的那台；那台給 hub 的金鑰是 `view` 時按「已確認」會提示要 `operate` 金鑰。
- hub 自己不存資料、只綁本機、不需要金鑰；要從別台看 hub，請對 hub 這台開 SSH 通道。
- 上游寫在 `~/.coding-lab/config.toml`（`[[hub.upstream]]`，含金鑰，檔案權限 600）；改埠用 `[hub] bind = "127.0.0.1:4802"` 或 `coding-lab hub --bind …`。上游網址只接受 `http://`：遠端一律走 tailnet（WireGuard 加密）或 SSH 通道。
- `curl 127.0.0.1:4802/healthz` 看每台連上了沒。

## 同一台機器上有好幾個專案

collector 收這台機器上所有 Claude Code 的事件，不分專案。頁面最上方的 **專案** 選單（只有一個專案時不顯示）決定怎麼看：

- **全部專案**（預設）：串流每行在 session id 前多一個專案標籤（專案名＝目錄的最後一段），檔案樹每個專案一段、點標題列可以收起，改動紀錄依專案分組；標題列右邊的 **⎇** 開那個專案的分支視窗。
- **單一專案**：只看這個專案的 session、串流、檔案樹、改動與注意佇列；其他專案的內容只是隱藏，切回「全部專案」就回來（注意佇列會提示其他專案還有幾件）。選擇會記在瀏覽器裡。

專案的歸類跟左欄一樣：session 的 cwd 在另一個 session 的 cwd 底下（worktree、子目錄）就算同一個專案。

## 看過去的 session（回放）

**回放一個 session**：

1. 網頁左欄上方切到 **Sessions** 分頁。正在跑的和已經結束的 session 都列在這裡，新的在上面，已結束的標「結束」。
2. 已結束的那一列右邊有 **▶ 回放**，按下會開出它的分頁，從存下來的紀錄讀回完整串流、碰過的檔案與改動、git 列，標題列標「回放」。
3. 串流捲到最上面按 **↑ 載入更多**，可以一路看到這個 session 的開頭。
4. 想放到第二個螢幕持續看，按分頁右上角的 **⧉ 監視器**。

**回放整個專案**（同一段時間裡所有 session 一起做了什麼）：

1. 中間欄最上面一排分頁的最右邊按 **⟲ 專案回放**。第一次打開會直接載入目前的專案、最近 3 小時。
2. 選專案（worktree 與子目錄裡的 session 算在同一個專案）與時間範圍：最近 3 小時、今天、昨天，或「自訂」填開始與結束後按 **載入**。
3. 所有 session（含已結束的）合成一條串流，跟「全部串流」一樣每行標 session 與 agent，可以用 session 與類型篩選。
4. 捲到最上面按 **↑ 載入更多** 看更早的，直到顯示「已到範圍開頭」。回放不會即時更新；要看新的動作回到「全部串流」。

紀錄保留 30 天，超過的會自動清掉。

## 更新

在 clone 下來的資料夾拉新的程式碼、重新編譯，再把執行檔與網頁蓋過去：

```bash
cd /path/to/VibingTracker
git pull
cargo build --release -p coding-lab
```

替換前要先讓 collector 停下來（Windows 不能覆蓋執行中的 .exe）。請它正常結束（寫完紀錄、放掉埠才回來；沒在跑也不會報錯）：

```bash
~/.coding-lab/bin/coding-lab stop              # Windows 在 Git Bash 用同一行
```

它不理會時（`stop` 會印出說明）才直接結束程序：`pkill coding-lab`（Linux、macOS）或 `taskkill /IM coding-lab.exe /F`（Windows）。直接結束不會弄壞紀錄：hook 事件一收到就寫進磁碟，transcript 的內容最多掉結束前 0.1 秒內還沒寫的那一批，下一個 session 開始時會重新拉起、從上次讀到的行接著把它們補回來。接著：

```bash
cp target/release/coding-lab ~/.coding-lab/bin/      # Windows：coding-lab.exe
rm -rf ~/.coding-lab/bin/web && cp -r web ~/.coding-lab/bin/
```

`plugin/` 底下有改動時（hook 設定或啟動腳本），再讓 Claude Code 重新讀 plugin：

```bash
claude plugin marketplace update coding-lab
claude plugin update coding-lab@coding-lab
```

plugin 的版本號還沒跟著每次改動調整，`update` 可能判斷成「已是最新」；這時直接重裝：`claude plugin uninstall coding-lab@coding-lab` 後再 `claude plugin install coding-lab@coding-lab`。之後開的 session 就會用新版。

## 疑難排解

**網頁打不開（<http://127.0.0.1:4801> 連不上）**

- 先跑 `coding-lab healthz`。連不上代表 collector 沒在跑：開一個**新的** session 讓 plugin 拉起它、跑一次 `coding-lab ensure`，或直接在終端機執行 `~/.coding-lab/bin/coding-lab serve`（前景執行，關掉終端機就停），看它印出什麼錯誤。剛用 `claude plugin install` 裝好的那一刻常見這個狀況：已開的 session 有送事件但沒人拉 collector。
- `[web].bind` 綁的是區網或 tailnet IP 時，`http://127.0.0.1:4801` 本來就連不上（SSH 通道也會 `Connection refused`）：改開那個 IP，或把 bind 寫成陣列同時綁兩個。
- 瀏覽器不在跑 Claude Code 的那台電腦上：見上面的 SSH 通道或 tailnet 直連。

**登入頁一直說金鑰不對**

- 金鑰只在 `key create` 時印一次，之後 `key list` 看到的是 id，不是金鑰；忘了就 `key revoke` 舊的再 `key create` 一把。
- `coding-lab key list` 的狀態是不是「已撤銷」或「已到期」。
- 連續失敗 10 次會被擋一分鐘（每個來源 IP 各算）。
- 伺服器的 `~/.coding-lab/collector.log` 會寫被拒的原因；`keys.json` 權限不是 600 時所有金鑰都會失效，照 log 裡的 `chmod 600` 修。

**SSH 通道進來也被要求登入**

伺服器的設定是 `[web] auth = "always"`（多人共用的伺服器）。這是刻意的：用金鑰登入一次即可。

**網頁顯示 404 或空白**

`web/` 沒放到執行檔旁邊。確認 `~/.coding-lab/bin/web/index.html` 存在；不存在就重做「編譯並放好 collector」的最後一行 `cp -r web ~/.coding-lab/bin/`。

**session 開了，網頁卻看不到它**

- 這個 session 是不是在裝 plugin 之前開的？CLI 不會載入新 hook，關掉重開一個；Desktop 會從下一個動作開始追蹤。
- 在 Claude Code 裡輸入 `/hooks`，要看得到 `SessionStart`（command）與其他事件（送到 `http://127.0.0.1:4800/hook`）。看不到就是 plugin 沒裝上，重做「安裝 plugin」。
- Windows：沒裝 Git for Windows 的話，`SessionStart` 的啟動腳本跑不起來，collector 不會被拉起。

**Claude Code 顯示 `hook error` 或 `ECONNREFUSED`**

collector 沒在跑時，Claude Code 照常運作、不會變慢，但可能顯示這類提示（例如 `PreToolUse hook error`）。開一個新的 session 會重新拉起 collector；不想看到提示可以先停用 plugin：`claude plugin disable coding-lab`。

**`SessionStart` 印出「找不到 collector 二進位，這次 session 不會被追蹤」**

plugin 找不到執行檔。它依序找：環境變數 `CODING_LAB_BIN`、`~/.coding-lab/bin/coding-lab`（或 `.exe`）、PATH 上的 `coding-lab`。確認執行檔放對位置後開一個新的 session。

**macOS 擋下執行檔（「無法打開，因為無法驗證開發者」）**

自己編譯的通常不會遇到。從別處複製來的執行檔可能帶隔離標記，移除它：

```bash
xattr -d com.apple.quarantine ~/.coding-lab/bin/coding-lab
```

**`SessionStart` 自動下載後印出「解壓 … 失敗」（Windows）**

標準的 Git for Windows 沒有 `unzip`。plugin 會退到 Windows 10 以後內建的 `C:\Windows\System32\tar.exe`，再退到 PowerShell 的 `Expand-Archive`；三個都沒有才會失敗。沒有自動下載也可以照「編譯並放好 collector」手動放好執行檔。

**Claude Code 顯示 `SessionStart hook timed out`（Windows）**

`SessionStart` 的啟動腳本要先開 Git Bash、再拉起 collector，上限 5 秒。Git Bash 冷啟動和 Windows Defender 第一次掃描新編好的執行檔可能超過。正常情況下這個 hook 約 0.6 秒，很少出現；偶爾一次不用理，每次都超時的話，把 `plugin/hooks/hooks.json` 裡 `SessionStart` 的 `timeout` 從 5 改成 10，重開 session。啟動腳本自己的錯誤寫在 `~/.coding-lab/ensure.log`，collector 的在 `collector.log`。

**collector 綁不到埠，每個 session 都顯示 `ECONNREFUSED`（Windows）**

Hyper-V 或 WSL 可能把 4800／4801 劃進保留範圍，collector 啟動就失敗（`collector.log` 會寫綁埠錯誤）。用這個指令查：

```powershell
netsh int ipv4 show excludedportrange protocol=tcp
```

4800 或 4801 落在列出的範圍裡時，改用別的埠：在 `~/.coding-lab/config.toml` 把 `[web].bind` 改成例如 `127.0.0.1:4811`；`[hook].bind` 目前不能改（plugin 固定送到 4800），只能重新整理保留範圍（`netsh int ipv4 set dynamicport tcp start=49152 num=16384`，要系統管理員權限，之後重開機）。

**看日誌**

collector 的日誌在 `~/.coding-lab/collector.log`（10 MB 輪替五份）。回報問題時附上最後幾十行與 `coding-lab healthz` 的輸出。

## 卸載

```bash
claude plugin uninstall coding-lab@coding-lab
claude plugin marketplace remove coding-lab
```

接著結束 collector（有裝常駐服務的先移除），再刪掉執行檔、網頁、設定與所有追蹤紀錄：

```bash
~/.coding-lab/bin/coding-lab service uninstall   # 只在裝過常駐服務時需要
~/.coding-lab/bin/coding-lab stop
rm -rf ~/.coding-lab
```

設定檔（`~/.coding-lab/config.toml`）、危險指令規則與單價表的格式見 [README 的「設定」](../README.md#設定)。
