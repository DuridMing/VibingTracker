"""tests/e2e/lifecycle.py 的比對邏輯（#688）：不呼叫模型，用手寫的最小 transcript 行與 Event 驗證。"""

import os
import shutil
import time
import unittest

import lifecycle
from lifecycle import compare

L_BIN_EXISTS = lifecycle.BIN.exists()


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

    @unittest.skipUnless(os.name != "nt", "用 sh 造假的背景程序，Windows 沒有")
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

    def test_alive_is_false_for_an_exited_process(self):
        # #711：Windows 的 os.kill(pid, 0) 對不存在的 pid 丟 OSError [WinError 87]，不是 ProcessLookupError
        import subprocess
        import sys
        p = subprocess.Popen([sys.executable, "-c", "pass"])
        p.wait()
        self.assertFalse(self.L.alive(p.pid))
        self.assertTrue(self.L.alive(os.getpid()))

    @unittest.skipUnless(L_BIN_EXISTS, "要有 target/release/coding-lab（tests/scripts/lifecycle.sh 會先建）")
    def test_stop_collector_stops_a_running_collector_without_signals(self):
        # #711：stop_collector 走 `coding-lab stop`（#700），三平台都通；Windows 沒有 SIGTERM 可送
        import subprocess
        L = self.L
        shutil.rmtree(L.HOME, ignore_errors=True)
        L.HOME.mkdir(parents=True)
        hook, web = L.free_port(), L.free_port()
        (L.HOME / "config.toml").write_text(
            f'[hook]\nbind = "127.0.0.1:{hook}"\n[web]\nbind = "127.0.0.1:{web}"\nroot = "{L.ROOT / "web"}"\n'
        )
        env = dict(os.environ, CODING_LAB_HOME=str(L.HOME))
        p = subprocess.Popen([str(L.BIN), "serve"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(100):
                if (L.HOME / "collector.json").exists():
                    break
                time.sleep(0.1)
            self.assertTrue((L.HOME / "collector.json").exists(), "collector 沒起來")
            L.stop_collector()
            self.assertFalse((L.HOME / "collector.json").exists(), "stop 之後 collector.json 要被刪掉")
            self.assertIsNotNone(p.wait(timeout=10))
        finally:
            if p.poll() is None:
                p.kill()

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
