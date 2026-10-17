"""tests/e2e/multi_sessions.py 的準備邏輯（#658）：不呼叫模型。"""

import shutil
import unittest

import multi_sessions as M


class Workdir(unittest.TestCase):
    def tearDown(self):
        shutil.rmtree(M.L.WORK / "s99", ignore_errors=True)

    def test_leftovers_from_a_previous_run_are_removed(self):
        # 上一次留下的 out-*.txt 會讓 Write 被 Claude Code 擋下（要先 Read），PreToolUse 不觸發、驗收誤判
        old = M.workdir(99)
        (old / "out-2.txt").write_text("a\nb\nc\n")
        cwd = M.workdir(99)
        self.assertFalse((cwd / "out-2.txt").exists())
        self.assertTrue((cwd / ".git").exists())
        self.assertEqual(len((cwd / "notes.txt").read_text().splitlines()), M.NOTES_LINES)


class HunkChangedLine(unittest.TestCase):
    def test_old_string_with_unchanged_neighbours(self):
        # 模型的 old_string 常帶上下相鄰的行：hunk 把整段標成 −／+，真正改的是第一個不同的那一行
        h = {"start": 1, "lines": [" line 1", "-line 2", "-line 3", "-line 4", "+line 2", "+edited 9", "+line 4", " line 5"]}
        self.assertEqual(M.hunk_changed_line(h), 3)

    def test_single_line_edit(self):
        h = {"start": 18, "lines": [" line 18", " line 19", " line 20", "-line 21", "+edited 1", " line 22"]}
        self.assertEqual(M.hunk_changed_line(h), 21)


if __name__ == "__main__":
    unittest.main()
