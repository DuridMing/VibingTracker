"""tests/e2e/lifecycle.py 的比對邏輯（#688）：不呼叫模型，用手寫的最小 transcript 行與 Event 驗證。"""

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


if __name__ == "__main__":
    unittest.main()
