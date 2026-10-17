"""tests/e2e/lifecycle.py 的比對邏輯（#688）：不呼叫模型，用手寫的最小 transcript 行與 Event 驗證。"""

import os
import unittest

from lifecycle import compare


def assistant(block, model="claude-opus-5-5"):
    return {"type": "assistant", "message": {"model": model, "content": [block]}}


def ev(kind, no):
    return {"type": kind, "line": {"file": "main", "no": no}, "payload": {}}


class Compare(unittest.TestCase):
    def test_synthetic_text_counts_on_both_sides(self):
        # Claude Code 自己寫的 <synthetic> 文字（例如 "No response requested."）merge 一樣會發 assistant.text
        lines = [
            assistant({"type": "text", "text": "OK"}),
            assistant({"type": "text", "text": "No response requested."}, model="<synthetic>"),
        ]
        _, missing, twice, extra = compare(lines, [ev("assistant.text", 1), ev("assistant.text", 2)])
        self.assertEqual((missing, twice, extra), ([], [], []))

    def test_missing_and_duplicate_lines_are_reported(self):
        lines = [assistant({"type": "tool_use", "id": "t1"}), assistant({"type": "thinking", "thinking": "x"})]
        _, missing, twice, _ = compare(lines, [ev("tool.placed", 1), ev("tool.placed", 1)])
        self.assertEqual(missing, [("assistant.thinking", 2)])
        self.assertEqual(twice, [("tool.placed", 1)])

    def test_tool_called_from_transcript_counts_as_placed(self):
        # hook 沒來（工具在 PreToolUse 前就被 Claude Code 擋下，例如 Write 一個沒讀過的既有檔，或 collector 停機中）：
        # transcript 先到時直接建帶 pos 的 tool.called，不會有 tool.placed（contracts.md §3）
        lines = [assistant({"type": "tool_use", "id": "t1"})]
        called = {"type": "tool.called", "pos": 3, "line": {"file": "main", "no": 1}, "payload": {}}
        _, missing, twice, extra = compare(lines, [called])
        self.assertEqual((missing, twice, extra), ([], [], []))
        staged = {"type": "tool.called", "pos": None, "line": None, "payload": {}}
        _, missing, _, _ = compare(lines, [staged])
        self.assertEqual(missing, [("tool.placed", 1)])


if __name__ == "__main__":
    unittest.main()


class Setup(unittest.TestCase):
    """setup() 不留下殺不掉、找不到的 collector（#688）。"""

    def setUp(self):
        import lifecycle as L
        self.L = L
        self.saved = (L.WORK, L.HOME, L.PLUGIN)
        L.WORK = L.ROOT / "target" / "tmp" / "e2e-lifecycle-unittest"
        L.HOME = L.WORK / "home"
        L.PLUGIN = L.WORK / "plugin"

    def tearDown(self):
        self.L.WORK, self.L.HOME, self.L.PLUGIN = self.saved

    def test_leftover_collector_is_stopped_before_its_home_is_wiped(self):
        import json
        import subprocess
        L = self.L
        L.HOME.mkdir(parents=True, exist_ok=True)
        # 上一次被 SIGKILL 的腳本留下的 collector：放到背景脫離這個程序（不是子程序，停掉後由系統回收）
        out = subprocess.run(["sh", "-c", "sleep 60 >/dev/null 2>&1 & echo $!"], capture_output=True, text=True)
        pid = int(out.stdout.strip())
        (L.HOME / "collector.json").write_text(json.dumps({"pid": pid}))
        try:
            L.setup()
            self.assertFalse(L.alive(pid), "舊的 collector 要先被停掉")
        finally:
            if L.alive(pid):
                os.kill(pid, 9)

    def test_collector_exits_on_its_own_when_idle(self):
        L = self.L
        L.setup()
        conf = (L.HOME / "config.toml").read_text()
        line = next(l for l in conf.splitlines() if l.startswith("idle_exit_seconds"))
        self.assertGreater(int(line.split("=")[1]), 0, "腳本被中斷時 collector 要能自己結束")

    def test_collector_serves_the_repo_web(self):
        # #656 之後網頁預設找二進位旁的 web/，target/release/ 底下沒有：測試用的 collector 要指到 repo 的 web/
        L = self.L
        L.setup()
        conf = (L.HOME / "config.toml").read_text()
        self.assertIn(f'root = "{L.ROOT / "web"}"', conf)
