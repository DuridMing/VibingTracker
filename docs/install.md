# 安裝手冊

這份手冊帶你從零裝好 coding-lab：編譯 collector、裝 Claude Code plugin、確認追蹤有在跑。目前還沒有發佈好的二進位，要從原始碼編譯，大約 10 分鐘（大部分是第一次編譯）。

裝好之後，每次開 Claude Code session 都會自動被追蹤，打開 <http://127.0.0.1:4801> 就能看到所有 session 合在一起的串流。追蹤不花 token，也不會攔截或改變 Claude Code 的行為。

## 目錄

1. [需要的東西](#需要的東西)
2. [編譯並放好 collector](#編譯並放好-collector)
3. [安裝 plugin](#安裝-plugin)
4. [確認有在追蹤](#確認有在追蹤)
5. [看過去的 session](#看過去的-session)
6. [更新](#更新)
7. [疑難排解](#疑難排解)
8. [卸載](#卸載)

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

1. **開一個新的 Claude Code session**（裝 plugin 之前就開著的 session 不會被追蹤）。CLI 或 Desktop 的 Code 分頁都可以。
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

## 看過去的 session

沒有另外的「回放」按鈕，過去的 session 從 **Sessions** 分頁打開：

1. 網頁左欄上方切到 **Sessions** 分頁。正在跑的和已經結束的 session 都列在這裡，新的在上面，已結束的標「已結束」。
2. 點一個 session，右邊會開出它的分頁：終端機式的完整串流、碰過的檔案與改動、git 列。
3. 已經結束一段時間（超過 2 小時）的 session，會從存下來的紀錄由最新往前讀回來；往上捲到頂端會自動載入更早的部分。
4. 想放到第二個螢幕持續看，按分頁右上角的 **⧉ 監視器**。

紀錄保留 30 天，超過的會自動清掉。

## 更新

在 clone 下來的資料夾拉新的程式碼、重新編譯，再把執行檔與網頁蓋過去：

```bash
cd /path/to/VibingTracker
git pull
cargo build --release -p coding-lab
```

替換前要先讓 collector 停下來（Windows 不能覆蓋執行中的 .exe）。等所有 session 結束 10 分鐘它會自己退出，或直接結束它：

```bash
pkill coding-lab                               # Linux、macOS
taskkill /IM coding-lab.exe /F                 # Windows（PowerShell 或命令提示字元）
```

直接結束不會弄壞紀錄，下一個 session 開始時會重新拉起，中間漏掉的部分會從 transcript 補回來。接著：

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

- 先跑 `coding-lab healthz`。連不上代表 collector 沒在跑：開一個**新的** session 讓 plugin 拉起它，或直接在終端機執行 `~/.coding-lab/bin/coding-lab serve`（前景執行，關掉終端機就停），看它印出什麼錯誤。
- 瀏覽器不在跑 Claude Code 的那台電腦上：見上面的 SSH 通道。

**網頁顯示 404 或空白**

`web/` 沒放到執行檔旁邊。確認 `~/.coding-lab/bin/web/index.html` 存在；不存在就重做「編譯並放好 collector」的最後一行 `cp -r web ~/.coding-lab/bin/`。

**session 開了，網頁卻看不到它**

- 這個 session 是不是在裝 plugin 之前開的？關掉重開一個。
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

**看日誌**

collector 的日誌在 `~/.coding-lab/collector.log`（10 MB 輪替五份）。回報問題時附上最後幾十行與 `coding-lab healthz` 的輸出。

## 卸載

```bash
claude plugin uninstall coding-lab@coding-lab
claude plugin marketplace remove coding-lab
```

collector 會在所有 session 結束、閒置 10 分鐘後自己退出（或用上面「更新」裡的指令結束它）。最後刪掉執行檔、網頁、設定與所有追蹤紀錄：

```bash
rm -rf ~/.coding-lab
```

設定檔（`~/.coding-lab/config.toml`）、危險指令規則與單價表的格式見 [README 的「設定」](../README.md#設定)。
