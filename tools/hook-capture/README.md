# hook-capture

階段 0 用的資料收集工具（OpenProject #624、#625）。不隨 coding-lab 發佈。

收三種東西，全部寫在輸出目錄（預設 `~/.cache/coding-lab-capture/<session_id>/`），**不寫進 repo**：

| 檔案 | 內容 |
| --- | --- |
| `hooks.jsonl` | 每筆 hook payload 原文＋`ts_seen_ms`＋`via`（`http` 或 `command`）。同一個事件兩條路各記一筆，用來比對 payload 差異與延遲 |
| `transcript-arrivals.jsonl` | `SessionStart` 後每 25 ms 輪詢 transcript，每一行新行記一筆：到達時間、行號、`type`、`uuid`、`timestamp`、`isSidechain`、`model`、content block 種類、`tool_use` id。**不存內容本身** |
| `transcript.jsonl`、`transcript-dir/` | `SessionEnd` 時複製的 transcript，以及同名子目錄（subagent 可能在這裡） |
| `meta.jsonl` | 每次 `SessionStart` 的 `source`、`transcript_path`、`cwd` |

`hooks.jsonl` 的 `ts_seen_ms` 和 `transcript-arrivals.jsonl` 的 `ts_seen_ms` 用同一個時鐘，可以直接比較 hook 與 transcript 誰先到。

## 用法

```bash
cargo build -p hook-capture --release
target/release/hook-capture serve            # 前景跑，127.0.0.1:4899；Ctrl-C 結束
```

另開一個**測試用目錄**（不要在正在開發的 repo 裡掛），只在那裡生效：

```bash
mkdir -p ~/capture-lab/.claude && cd ~/capture-lab && git init
/mnt/data/Project/VibingTracker/target/release/hook-capture settings > .claude/settings.local.json
claude
```

`settings` 會掛上 Claude Code 文件列出的全部 33 個事件，每個事件同時有 http 與 command 兩個 hook，都不加 matcher。hook 一律回 200 空 body、command 版不輸出任何東西，所以不會影響 Claude Code 的行為。

- `serve` 沒在跑時，http hook 連不上只算非阻斷錯誤，command hook 仍會照常記錄。
- 程序被砍、沒觸發 `SessionEnd` 時，用 `hook-capture snapshot <session_id>` 手動複製 transcript。
- 輸出目錄可用 `--out` 或 `HOOK_CAPTURE_OUT` 改；`cmd` 只看 `HOOK_CAPTURE_OUT`。

## 建議的情境

一個 session 內依序做，盡量讓每種事件都出現：

1. 要求一個需要思考的修改：Read → Edit → Write 新檔
2. Bash 成功一次、失敗兩次（例如跑不存在的指令），再 `git commit`
3. 叫一個 subagent（Task）去搜尋檔案
4. 讓它用 AskUserQuestion 問你問題；停著不回，等 `idle_prompt`
5. 觸發一次權限詢問（`permission_prompt`）
6. `/compact`，再結束；之後 `claude --resume` 接回同一個 session

## 進 `samples/` 前

原始輸出含路徑、prompt、檔案內容，**不能直接 commit**。挑選後先遮蔽家目錄路徑、使用者名稱、token 與私人內容，再放進 `samples/hooks/`、`samples/transcripts/`。
