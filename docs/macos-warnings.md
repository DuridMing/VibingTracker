# macOS 注意事項

macOS 的安全與隱私機制會在幾個地方擋住或干擾 coding-lab。這份文件列出你可能遇到的狀況、原因與處理方式。安裝步驟本身見[安裝手冊](install.md)，日常操作見[使用手冊](usage.md)。

> 這份文件依程式碼與 macOS 的一般行為整理，**尚未在 Mac 實機逐項驗證**。遇到和這裡描述不同的情況，請附上 macOS 版本、晶片（Apple Silicon 或 Intel）與 `~/.coding-lab/collector.log` 最後幾行回報。

## 快速對照

| 你看到的 | 原因 | 見 |
| --- | --- | --- |
| 「無法打開 coding-lab，因為無法驗證開發者」 | Gatekeeper 擋下從網路下載的執行檔 | [1](#1-gatekeeper無法驗證開發者) |
| 終端機只印 `killed` 或 `zsh: killed` | 執行檔的簽章壞掉（Apple Silicon） | [2](#2-apple-silicon執行就被-killed) |
| 跳出「要安裝命令列開發者工具嗎？」 | 沒有 Xcode Command Line Tools，collector 呼叫 git 時系統跳出安裝提示 | [3](#3-一直跳出安裝命令列開發者工具) |
| 「coding-lab 想要取用『文件』檔案夾中的檔案」，或檔案樹是空的 | 檔案與檔案夾的隱私保護（TCC） | [4](#4-專案在桌面文件下載或-icloud-雲碟裡) |
| 「允許在背景執行」通知；常駐服務裝了卻沒在跑 | macOS 13 以後的「登入項目」管理 | [5](#5-常駐服務允許在背景執行) |
| `service install` 失敗，訊息有 `Bootstrap failed: 5` | LaunchAgent 已經載入 | [6](#6-service-install-出現-bootstrap-failed-5) |
| 「要允許 coding-lab 接受傳入的網路連線嗎？」 | macOS 防火牆（只在綁區網／tailnet 位址時） | [7](#7-防火牆詢問傳入連線) |
| 在 `~/.zshrc` 設的環境變數沒作用 | 從 Dock 開的 App 與 LaunchAgent 不讀 shell 設定 | [8](#8-環境變數沒有作用) |
| 蓋上筆電後遠端看不到、hub 標「離線」 | Mac 睡眠時 collector 也暫停 | [9](#9-睡眠) |
| 瀏覽器通知沒出現 | macOS 的通知設定或專注模式 | [10](#10-瀏覽器通知沒出現) |

---

## 1. Gatekeeper：「無法驗證開發者」

**會遇到的情況**：用瀏覽器下載別人編好的 `coding-lab`（或壓縮檔），解開後執行。自己用 `cargo build` 編的不會遇到；plugin 用 `curl` 自動下載的通常也不會。

**原因**：coding-lab 的執行檔沒有 Apple 開發者簽章與公證。瀏覽器下載的檔案會帶「隔離」標記，Gatekeeper 看到就擋下。

**處理**：確認檔案來源可信後，移除隔離標記：

```bash
xattr -d com.apple.quarantine ~/.coding-lab/bin/coding-lab
```

或打開「系統設定 → 隱私權與安全性」，在下方找到被擋下的 coding-lab，按「強制打開」（macOS 15 起不能再用右鍵「打開」繞過）。

`web/` 資料夾裡的檔案不需要處理，它們只被 collector 讀取。

## 2. Apple Silicon：執行就被 `killed`

**會遇到的情況**：M 系列晶片的 Mac，執行 `coding-lab` 時終端機只印 `killed` 或 `zsh: killed`，沒有其他訊息。

**原因**：Apple Silicon 上的執行檔一定要有簽章（至少是本機的 ad-hoc 簽章）。Rust 編譯時會自動簽，但事後用 `strip` 等工具修改過檔案，或複製時檔案損毀，簽章就失效，系統直接結束它。

**處理**：重新做 ad-hoc 簽章：

```bash
codesign --force --sign - ~/.coding-lab/bin/coding-lab
```

**另外注意 Rosetta**：如果你的終端機是用 Rosetta 開的（`uname -m` 印出 `x86_64`），plugin 自動下載時會選 Intel 版，能跑但比較慢。在原生終端機裡（`uname -m` 是 `arm64`）重新安裝即可。

## 3. 一直跳出「安裝命令列開發者工具」

**會遇到的情況**：沒有裝 Xcode Command Line Tools 的 Mac（例如用下載的執行檔，沒有自己編譯），打開網頁的檔案樹或分支視窗時，系統反覆跳出「`git` 指令需要命令列開發者工具，要現在安裝嗎？」。

**原因**：collector 用 `git` 取得檔案清單與改動。沒有裝開發者工具時，`/usr/bin/git` 只是一個會跳出安裝提示的替身，每次被呼叫都會跳。

**處理**：安裝開發者工具（裝好後 git 也有了）：

```bash
xcode-select --install
```

## 4. 專案在「桌面」「文件」「下載」或 iCloud 雲碟裡

**會遇到的情況**：

- 跳出「coding-lab 想要取用『文件』檔案夾中的檔案」之類的提示；
- 或者沒有提示，但網頁上這個專案的檔案樹是空的、改動沒有行號與 diff。

**原因**：macOS 把「桌面」「文件」「下載」、iCloud 雲碟、外接與網路磁碟視為受保護的位置。collector 會讀專案裡被修改的檔案（算行號）並在專案目錄跑 `git`，所以需要權限：

- collector 由 Claude Code 自動拉起時，用的是啟動 Claude Code 的那個 App（「終端機」、iTerm、Claude Desktop）的權限。那個 App 已經能存取專案的話，通常不會再問。
- 裝成**常駐服務**（`service install`）後，collector 由系統的 launchd 啟動，權限算在 coding-lab 自己身上，會另外詢問或直接被拒。
- 沒有開發者簽章的執行檔，**每次更新後**系統都會把它當成新的程式，之前給的權限可能失效、要重新允許。

**處理**（擇一）：

- 把專案移到這些位置以外，例如 `~/src/`、`~/code/`（最省事）。
- 跳出提示時按「好」。被拒過的話，到「系統設定 → 隱私權與安全性 → 檔案與檔案夾」打開 coding-lab 的對應項目。
- 常駐服務模式要穩定存取的話，在「系統設定 → 隱私權與安全性 → 完全取用磁碟」加入 `~/.coding-lab/bin/coding-lab`（權限很大，請自行評估；更新執行檔後可能要重新加入）。

transcript 在 `~/.claude/`、collector 的資料在 `~/.coding-lab/`，都不在受保護的位置，不受這點影響。

## 5. 常駐服務：「允許在背景執行」

**會遇到的情況**：執行 `coding-lab service install` 後跳出「背景項目已加入」或「允許在背景執行」的通知；或者之後發現服務裝了，collector 卻沒在跑。

**原因**：常駐服務是一個 LaunchAgent（`~/Library/LaunchAgents/io.coding-lab.collector.plist`）。macOS 13 起，「系統設定 → 一般 → 登入項目與延伸功能」可以關掉任何背景項目；被關掉的 LaunchAgent 不會啟動。

**處理**：

1. 打開「系統設定 → 一般 → 登入項目與延伸功能」，在「允許在背景執行」裡確認 coding-lab（可能顯示為 `coding-lab` 或 `io.coding-lab.collector`）是開著的。
2. 用下面的指令確認：

   ```bash
   ~/.coding-lab/bin/coding-lab service status
   launchctl print gui/$(id -u)/io.coding-lab.collector | head -20
   ```

3. 服務的早期錯誤寫在 `~/.coding-lab/service.log`，平常的日誌在 `~/.coding-lab/collector.log`。

服務只在**你登入之後**執行（LaunchAgent 屬於登入的使用者）；Mac 開機但還沒有人登入時不會跑。

## 6. `service install` 出現 `Bootstrap failed: 5`

**會遇到的情況**：重裝常駐服務時失敗，訊息含 `Bootstrap failed: 5: Input/output error`。

**原因**：這個 LaunchAgent 已經載入。`service install` 只在 plist 檔還在時才會先卸載舊的；如果 plist 被手動刪掉、服務卻還在載入狀態，就會撞上。

**處理**：手動卸載再裝一次：

```bash
launchctl bootout gui/$(id -u)/io.coding-lab.collector
~/.coding-lab/bin/coding-lab service install
```

## 7. 防火牆詢問傳入連線

**會遇到的情況**：把 `[web].bind` 設成區網或 tailnet 位址（給別台或手機看）之後，跳出「要允許 coding-lab 接受傳入的網路連線嗎？」。

**原因**：macOS 防火牆開著時，沒有簽章的程式第一次在非本機位址上聽連線，系統會問。只綁 `127.0.0.1`（預設）不會問；用 SSH 通道看也不會問。

**處理**：

- 要從別台直連就按「允許」；按了「拒絕」的話，到「系統設定 → 網路 → 防火牆 → 選項」把 coding-lab 改成允許。
- **每次更新執行檔後可能會再問一次**（系統把它當成新的程式）。
- 不想處理防火牆的話，維持預設的 `127.0.0.1`，改用 SSH 通道：`ssh -N -L 4801:127.0.0.1:4801 <這台 Mac>`。

## 8. 環境變數沒有作用

**會遇到的情況**：在 `~/.zshrc` 設了 `CODING_LAB_HOME`、`CODING_LAB_BIN`、`CODING_LAB_RELEASE_URL` 等，用終端機開的 `claude` 有作用，但 Claude Desktop 或常駐服務好像沒讀到。

**原因**：從 Dock 或啟動台開的 App（例如 Claude Desktop），以及 launchd 啟動的常駐服務，都**不會**讀 `~/.zshrc`、`~/.zprofile` 這些 shell 設定檔。

**處理**：

- collector 的設定（埠、網頁位置、驗證方式等）寫進 `~/.coding-lab/config.toml`，不要只靠環境變數；這個檔不論誰啟動都會讀。
- 執行檔放在預設的 `~/.coding-lab/bin/`，就不需要 `CODING_LAB_BIN`。
- 資料目錄維持預設的 `~/.coding-lab`，不要用 `CODING_LAB_HOME` 改到別處；否則從終端機與從 Desktop 開的 session 可能寫到不同地方。

## 9. 睡眠

**會遇到的情況**：Mac 睡眠（例如蓋上筆電）後，從別台或手機看不到；hub 把這台標成「離線」。

**原因**：睡眠時所有程式都暫停，collector 也一樣。醒來後會自動恢復，hub 也會自動重連補齊，紀錄不會遺失。

**處理**：需要一直從遠端看的話，接上電源並在「系統設定 → 電池（或能源）」設定接電源時不要自動睡眠，或暫時用 `caffeinate -s` 讓它保持清醒。

## 10. 瀏覽器通知沒出現

**會遇到的情況**：網頁上已經打開「瀏覽器通知」，卻沒有跳出通知。

**原因與處理**：

- 「系統設定 → 通知」裡，你用的瀏覽器（Safari、Chrome、Arc…）要允許通知。
- 「專注模式」（勿擾）開著時通知會被收起來。
- Safari 會針對每個網站另外詢問；被拒過的話到「Safari → 設定 → 網站 → 通知」改回允許。
- 只有一個主視窗會發通知；監視器視窗不發。

## 其他小提醒

- **Time Machine**：追蹤紀錄在 `~/.coding-lab/events/`，會被 Time Machine 一起備份。不想備份的話：`tmutil addexclusion ~/.coding-lab`。
- **多個使用者帳號同時登入的 Mac**：4800、4801 是整台機器共用的。先啟動的帳號的 collector 佔住埠之後，另一個帳號的 collector 起不來；那個帳號的 hook 會被先啟動的 collector 認出不是自己的而丟掉，所以**後來的帳號不會被追蹤**（`coding-lab healthz` 印「沒在跑」，`collector.log` 有一行說明）。兩個帳號都要用時，用「快速切換使用者」前先在另一個帳號執行 `coding-lab stop`；一台機器同時只能有一個帳號被追蹤（見[安裝手冊](install.md#確認有在追蹤)）。（這點不限 macOS，Linux 的多人伺服器也一樣。）
- **卸載**：先 `coding-lab service uninstall`（裝過常駐服務時），再照[安裝手冊「卸載」](install.md#卸載)。「登入項目」裡的 coding-lab 會隨之消失；沒有的話手動關掉即可。
