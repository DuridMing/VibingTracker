# 用 AI 安裝 coding-lab

這份文件分兩部分：

- **給人看的**：複製一段話貼給 Claude Code（或其他會跑終端機指令的 AI 助理），它就會照下面的步驟幫你裝好。
- **給 AI 看的**：AI 要照做的步驟、每一步怎麼確認成功、出錯怎麼處理、哪些事要先問你。

想自己動手裝的話，看[安裝手冊](install.md)。

---

## 給人看的：貼這段話給 AI

開一個終端機，啟動 Claude Code（`claude`），把下面其中一段貼進去。

**還沒有下載原始碼：**

```text
請讀 https://raw.githubusercontent.com/DuridMing/VibingTracker/main/docs/install-with-ai.md ，
照其中「給 AI 看的」步驟幫我安裝 coding-lab。安裝系統軟體或覆蓋檔案前先問我。
```

**已經 clone 下來了**（在那個資料夾裡啟動 Claude Code）：

```text
請讀 docs/install-with-ai.md，照其中「給 AI 看的」步驟幫我安裝 coding-lab。安裝系統軟體或覆蓋檔案前先問我。
```

過程中你可能要做的事：

- AI 問你要不要安裝缺少的工具（Rust、Git for Windows、Visual Studio Build Tools）時回答。需要系統管理員權限的安裝，AI 會請你自己執行。
- 第一次編譯大約 3～10 分鐘。
- 裝好之後**開一個新的 Claude Code session**：目前這個 session 不會載入剛裝好的 plugin。
- 瀏覽器打開 <http://127.0.0.1:4801>。

---

## 給 AI 看的

你要在使用者的電腦上安裝 coding-lab：一個 Rust 寫的 collector（`coding-lab`），加上一個 Claude Code plugin。plugin 會在每個 session 開始時拉起 collector，並把 hook 事件送給它；collector 在 <http://127.0.0.1:4801> 提供網頁。

### 規則

1. **先列計畫再動手**：開始前用幾行告訴使用者你會做哪些步驟、會寫到哪些地方（`~/.coding-lab/bin/`、Claude Code 的 plugin 設定、clone 的資料夾）。
2. **這些事要先問使用者，得到同意才做**：
   - 安裝系統軟體（Rust、Git、Visual Studio Build Tools、Claude Code CLI）；
   - 覆蓋已經存在的 `~/.coding-lab/bin/`（舊版本）；
   - 選擇 clone 的位置（沒有指定時建議 `~/src/VibingTracker`）；
   - 步驟 8 的自動驗證（會花少量 token）。
3. **不要做的事**：
   - 不要刪除 `~/.coding-lab/` 底下 `bin/` 以外的任何東西，那裡是使用者的追蹤紀錄、金鑰與設定；
   - 不要建立金鑰、不要把網頁綁到 127.0.0.1 以外的位址、不要安裝常駐服務，除非使用者要求（見「選用步驟」）；
   - 需要 `sudo` 或系統管理員權限的指令，請使用者自己執行。
4. **用 bash 執行指令**。Windows 要在 **Git Bash** 裡執行（Claude Code 在 Windows 上的 Bash 工具就是 Git Bash）。下面的 `~` 在 Windows 上是 `%USERPROFILE%`。
5. **每一步都要確認「成功條件」**。不符合時先查「疑難排解」；還是解決不了就停下來，把指令、輸出和你的判斷告訴使用者。
6. 編譯要好幾分鐘，可以放到背景執行，但要等它結束、確認成功再繼續。

### 步驟 1：偵測環境

```bash
uname -s
ls -la ~/.coding-lab/bin/ 2>/dev/null
claude plugin list 2>&1
```

- `uname -s` 是 `Linux`、`Darwin`（macOS）或 `MINGW64_NT-…`／`MSYS_NT-…`（Windows 的 Git Bash）。Windows 上若在 PowerShell 或 cmd 裡，改用 Git Bash。
- `~/.coding-lab/bin/` 已經有 `coding-lab`（或 `coding-lab.exe`）：這是**更新**。後面步驟照做即可，只是步驟 5 會先停掉舊的 collector。
- `claude plugin list` 已經列出 `coding-lab@coding-lab`：步驟 6 改成重裝。

### 步驟 2：檢查前置工具

```bash
git --version
cargo --version
claude --version
```

| 缺少 | 怎麼裝（先問使用者） |
| --- | --- |
| Git | Linux 用套件管理員；macOS 執行 `xcode-select --install`；Windows 安裝 [Git for Windows](https://git-scm.com/download/win)（**必裝**：plugin 的啟動腳本由 Git Bash 執行） |
| Rust（`cargo`） | [rustup](https://rustup.rs)。Linux／macOS：`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh -s -- -y`，裝完執行 `. "$HOME/.cargo/env"`。Windows：請使用者下載執行 `rustup-init.exe`，選預設的 MSVC |
| Visual Studio Build Tools（只有 Windows） | 安裝「使用 C++ 的桌面開發」工作負載（要系統管理員權限，請使用者自己裝）。沒有它，`cargo build` 會出現 `linker 'link.exe' not found` |
| Claude Code CLI | Linux／macOS：`curl -fsSL https://claude.ai/install.sh \| bash`；Windows PowerShell：`irm https://claude.ai/install.ps1 \| iex`。只裝了 Claude Desktop 也要裝 CLI，因為安裝 plugin 要用 CLI |

**成功條件**：三個指令都印出版本。剛裝好工具的話，可能要開新的終端機，PATH 才會更新。

**只有 Linux**：確認有 C 編譯器（SQLite 是隨 Rust 一起編譯的，需要它）：

```bash
cc --version || gcc --version
```

沒有的話請使用者安裝（要 `sudo`）：

| 發行版 | 指令 |
| --- | --- |
| Debian／Ubuntu | `sudo apt install build-essential` |
| Fedora／RHEL | `sudo dnf install gcc` |
| Arch | `sudo pacman -S base-devel` |
| Alpine | `sudo apk add build-base` |

**只有 Windows**：確認 4800、4801 沒有被 Hyper-V／WSL 劃進保留範圍：

```bash
netsh int ipv4 show excludedportrange protocol=tcp
```

如果 4800 或 4801 落在任何一列的開始到結束之間，停下來告訴使用者（見疑難排解「埠在保留範圍內」），先不要繼續。

### 步驟 3：取得原始碼

- 目前目錄已經是這個 repo（`git remote -v` 含 `VibingTracker`）：就用它，執行 `git switch main && git pull --ff-only`。
- 否則問使用者要放哪裡（建議 `~/src/VibingTracker`），然後執行：

```bash
mkdir -p ~/src && cd ~/src
git clone -b main https://github.com/DuridMing/VibingTracker.git
cd VibingTracker
```

**成功條件**：`git branch --show-current` 是 `main`，而且 `plugin/hooks/hooks.json` 存在。記下這個資料夾的**絕對路徑**（`pwd`；Windows 的 Git Bash 用 `pwd -W`，得到 `C:/…` 的寫法），步驟 6、8 會用到；**這個資料夾裝好後不能刪**。

`main` 是發佈的版本，`develop` 是開發中的版本。

### 步驟 4：編譯

```bash
cargo build --release -p coding-lab
```

**成功條件**：最後一行是 `Finished`，而且有 `target/release/coding-lab`（Windows 是 `target/release/coding-lab.exe`）。

### 步驟 5：放好執行檔與網頁

更新時要先停掉舊的 collector（Windows 不能覆蓋執行中的 .exe）。第一次安裝時什麼都不做；collector 沒在跑時 `stop` 只印一句，也是成功：

```bash
if [ -x ~/.coding-lab/bin/coding-lab ] || [ -x ~/.coding-lab/bin/coding-lab.exe ]; then
  ~/.coding-lab/bin/coding-lab stop
fi
```

接著放檔案（**覆蓋舊版前要問過使用者**）：

```bash
mkdir -p ~/.coding-lab/bin
cp target/release/coding-lab ~/.coding-lab/bin/          # Windows：cp target/release/coding-lab.exe ~/.coding-lab/bin/
rm -rf ~/.coding-lab/bin/web && cp -r web ~/.coding-lab/bin/
```

**成功條件**：`~/.coding-lab/bin/coding-lab --version`（Windows 可省略 `.exe`）印出版本，而且 `~/.coding-lab/bin/web/index.html` 存在。

### 步驟 6：安裝 plugin

把 `<repo>` 換成步驟 3 記下的絕對路徑（Windows 例如 `C:/Users/me/src/VibingTracker`）：

```bash
claude plugin marketplace add "<repo>"
claude plugin install coding-lab@coding-lab
```

- marketplace 已經加過（訊息說已存在）：改成 `claude plugin marketplace update coding-lab`。
- plugin 已經裝過（更新的情況）：先 `claude plugin uninstall coding-lab@coding-lab` 再 `install`。plugin 的版本號沒有隨每次改動調整，`update` 可能判斷成「已是最新」。
- **一定要用本機資料夾**。直接給 GitHub 的 `DuridMing/VibingTracker` 會讀到 `main`，那裡沒有 plugin。

**成功條件**：`claude plugin list` 列出 `coding-lab@coding-lab`，而且是啟用狀態。

### 步驟 7：拉起 collector

plugin 只會在**新的** session 開始時拉起 collector。先手動拉起一次，讓網頁馬上能開：

```bash
~/.coding-lab/bin/coding-lab ensure < /dev/null
~/.coding-lab/bin/coding-lab healthz
```

**成功條件**：`healthz` 印出一段 JSON，含 `"version"`。

### 步驟 8：驗證追蹤（先問使用者，會花少量 token）

開一個新的、非互動的 Claude Code session，讓它用一次工具，再確認 collector 收到了：

```bash
cd "<repo>" && claude -p "讀 README.md 的第一行，只回覆那一行" < /dev/null
curl -s http://127.0.0.1:4801/api/sessions
```

**成功條件**：`/api/sessions` 回傳的 `sessions` 裡，有一筆的 `cwd` 是這個資料夾。

使用者不同意的話跳過，改成在回報裡請使用者自己開新 session 驗證。

### 步驟 9：回報

用下面的格式告訴使用者：

```text
coding-lab 已安裝：
- 執行檔：~/.coding-lab/bin/coding-lab（版本 x.y.z）
- 原始碼：<repo>（main，請不要刪除這個資料夾）
- plugin：coding-lab@coding-lab（已啟用）
- collector：執行中（healthz 正常）
- 驗證：已確認新 session 有被追蹤／尚未驗證

接下來：
1. 開一個新的 Claude Code session（目前這個 session 不會被追蹤）。
2. 瀏覽器打開 http://127.0.0.1:4801。
3. 之後 collector 會在每個 session 開始時自動拉起，所有 session 結束、閒置 10 分鐘後自動退出。
更新、疑難排解、卸載見 docs/install.md。
```

### Linux 的特殊情況

步驟 1 判斷出是 Linux 時，再確認下面幾件事，回報時一併說明：

- **沒有桌面、沒有瀏覽器的伺服器**（SSH 登入、`$DISPLAY` 與 `$WAYLAND_DISPLAY` 都是空的）：網頁要從使用者自己的電腦看。回報的「接下來」改成：在自己的電腦執行 `ssh -N -L 4801:127.0.0.1:4801 <這台>`，再開 <http://127.0.0.1:4801>；用 VS Code Remote 的話，在「連接埠」面板轉送 4801。不要為此把網頁綁到 127.0.0.1 以外的位址。
- **WSL**（`uname -r` 含 `microsoft` 或 `WSL`）：照 Linux 安裝，Claude Code 也要在 WSL 裡執行；裝在 Windows 那邊的 Claude Code 不會被 WSL 裡的 plugin 追蹤（反之亦然）。WSL2 預設會把 `localhost` 轉給 Windows，Windows 的瀏覽器直接開 <http://127.0.0.1:4801> 即可；開不了的話改用 WSL 的 IP（`hostname -I`）並告訴使用者。
- **多人共用的伺服器**：其他使用者也能連到這台的 127.0.0.1:4801。提醒使用者可以在 `~/.coding-lab/config.toml` 加 `[web] auth = "always"`，連本機也要金鑰（金鑰由使用者決定要不要建）。
- **想要開機就跑**（伺服器重開機後不必先開 session）：選用步驟的 `service install` 在 Linux 是 systemd **使用者** unit；沒人登入時也要跑，還要使用者自己執行一次 `sudo loginctl enable-linger $USER`。
- **`~/.cargo/bin` 不在 PATH**（剛用 rustup 裝好）：執行 `. "$HOME/.cargo/env"` 再繼續；回報時提醒使用者重開終端機。

### 選用步驟（只在使用者要求時做）

| 使用者想要 | 做法 |
| --- | --- |
| 開機或登入後 collector 就在跑 | `~/.coding-lab/bin/coding-lab service install`，再用 `service status` 確認。Linux 是 systemd 使用者 unit（沒人登入也要跑時見上一節的 linger）；macOS 是 LaunchAgent，第一次會跳「允許在背景執行」；Windows 是工作排程器裡以使用者命名的工作（`\CodingLab\<使用者>`），登入時在背景啟動、不開視窗 |
| 從別台電腦或手機看 | 先建議 SSH 通道：`ssh -N -L 4801:127.0.0.1:4801 <這台>`。要直連時照 [安裝手冊「確認有在追蹤」](install.md#確認有在追蹤) 的 tailnet 步驟設定 `[web].bind` 與金鑰；金鑰只印一次，請使用者自己保存 |
| 一次看多台 | 照 [安裝手冊「一次看多台伺服器（hub）」](install.md#一次看多台伺服器hub) |

### 疑難排解

| 現象 | 原因 | 處理 |
| --- | --- | --- |
| `cargo build` 出現 `linker 'link.exe' not found`（Windows） | 沒有 Visual Studio Build Tools | 請使用者安裝「使用 C++ 的桌面開發」，開新終端機後重新編譯 |
| `cargo build` 出現 `linker 'cc' not found`（Linux） | 沒有 C 編譯器 | 請使用者安裝 `build-essential`（Debian／Ubuntu）或 `gcc`（其他發行版） |
| `cp: cannot create regular file … Device or resource busy`／`Permission denied`（Windows） | 舊的 collector 還在跑 | 執行步驟 5 的 `stop`；不理會時 `taskkill //IM coding-lab.exe //F`（Git Bash 要寫兩個斜線），再複製 |
| `claude plugin install` 找不到 `coding-lab@coding-lab` | marketplace 加的不是本機資料夾，或路徑錯 | `claude plugin marketplace list` 檢查；移除後用絕對路徑重加 |
| `healthz` 印出「collector 沒有在跑」 | 拉起失敗 | 看 `~/.coding-lab/collector.log` 最後幾行；或前景執行 `~/.coding-lab/bin/coding-lab serve` 看錯誤（看完按 Ctrl-C，再做一次步驟 7） |
| log 寫綁不到 4800／4801，或「埠在保留範圍內」（Windows） | Hyper-V／WSL 保留了這些埠 | 停下來告訴使用者：網頁埠可以在 `~/.coding-lab/config.toml` 改 `[web] bind = "127.0.0.1:4811"`；4800 不能改，只能由使用者以系統管理員身分重新設定保留範圍（見 [安裝手冊](install.md#疑難排解)） |
| 4800 或 4801 被別的程式佔用 | 其他程式在用 | `netstat -ano \| grep -E ':480[01] '`（Windows）或 `ss -ltnp \| grep 480`（Linux）找出程式，告訴使用者 |
| 步驟 8 的 `/api/sessions` 沒有新 session | plugin 沒載入，或 SessionStart 跑不起來 | 確認步驟 6 的成功條件；看 `~/.coding-lab/ensure.log`；Windows 確認裝了 Git for Windows |
| 網頁 404 或空白 | `web/` 沒放在執行檔旁邊 | 重做步驟 5 的最後一行 |
| macOS 擋下執行檔（無法驗證開發者） | 檔案帶隔離標記 | `xattr -d com.apple.quarantine ~/.coding-lab/bin/coding-lab` |

### 卸載（只在使用者要求時做）

```bash
claude plugin uninstall coding-lab@coding-lab
claude plugin marketplace remove coding-lab
~/.coding-lab/bin/coding-lab service uninstall   # 只在裝過常駐服務時需要
~/.coding-lab/bin/coding-lab stop
```

到這裡追蹤已經停止，紀錄還留著。要連紀錄、金鑰與設定一起刪掉（`rm -rf ~/.coding-lab`），**一定要再跟使用者確認一次**，刪了無法復原。
