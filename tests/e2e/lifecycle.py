#!/usr/bin/env python3
"""resume／compact 接回的真實操作驗收（#651）：同一個 session 用 `claude -p` 開始，再 resume 三次、compact 三次，
和 Claude Code 寫的 transcript 逐行比對 collector 的 JSONL。

順序：startup → (resume 執行 Bash → resume 執行 /compact) × 3。每次 resume 都讓 Claude 呼叫 Bash，
compact 之後的行也要讀到。檢查：
- session.started 的 source 依序是 startup、resume、resume、compact、resume、resume、compact…；compact.started／finished 各三次；
- transcript 裡每一行 assistant 的 thinking／text 恰好一個對應的 Event、每個 tool_use 恰好一個 tool.placed（stream），
  沒有重複、沒有缺漏；pos 嚴格遞增；沒有降級；
- `/api/sessions/<id>/events` 的串流裡有 resume 與 compact 的狀態行（session.started），次數與上面相同。

hook 的 url 寫死 127.0.0.1:4800，所以用的是正在跑的 collector；web 埠從它的 collector.json 讀。會實際呼叫模型（少量 token）。

用法：tests/e2e/lifecycle.py [CODING_LAB_HOME，預設 <repo>/.coding-lab]
"""

import json
import os
import subprocess
import sys
import time
import urllib.request
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BIN = ROOT / "target" / "release" / "coding-lab"
WORK = ROOT / "target" / "tmp" / "e2e-lifecycle"
ROUNDS = 3


def claude(home, args, prompt):
    env = dict(os.environ, CODING_LAB_BIN=str(BIN), CODING_LAB_HOME=str(home))
    r = subprocess.run(
        ["claude", "-p", "--plugin-dir", str(ROOT / "plugin"), "--allowedTools", "Bash(echo:*)", *args],
        input=prompt.encode(), env=env, cwd=WORK, capture_output=True, timeout=600,
    )
    print(f"  claude {' '.join(args[:1])} {prompt[:30]!r} → exit {r.returncode}：{r.stdout.decode().strip()[:60]!r}")
    if r.returncode != 0:
        print(r.stderr.decode())
    return r.returncode


def events_of(home, sid):
    p = home / "events" / f"{sid}.jsonl"
    return [json.loads(l) for l in p.read_text().splitlines()] if p.exists() else []


def main():
    home = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / ".coding-lab"
    info = json.loads((home / "collector.json").read_text())
    web = info["web"]
    WORK.mkdir(parents=True, exist_ok=True)
    if not (WORK / ".git").exists():  # 自己的 git root，避免讀到上層的 .claude/settings.local.json
        subprocess.run(["git", "init", "-q", str(WORK)], check=True)

    sid = str(uuid.uuid4())
    print(f"session {sid}")
    codes = [claude(home, ["--session-id", sid], "用 Bash 執行 echo start，然後只回答 OK")]
    expected = ["startup"]
    for i in range(1, ROUNDS + 1):
        codes.append(claude(home, ["--resume", sid], f"用 Bash 執行 echo resume-{i}，然後只回答 OK"))
        expected.append("resume")
        codes.append(claude(home, ["--resume", sid], "/compact"))
        expected += ["resume", "compact"]

    # 最後一個 SessionEnd 之後 tail 會讀完再停；等 JSONL 不再變
    last = -1
    for _ in range(50):
        n = len(events_of(home, sid))
        if n == last and n > 0:
            break
        last = n
        time.sleep(0.3)
    evs = events_of(home, sid)
    problems = []
    if any(codes):
        problems.append(f"有 claude 結束碼不是 0：{codes}")

    sources = [e["payload"].get("source") for e in evs if e["type"] == "session.started"]
    if sources != expected:
        problems.append(f"session.started 的 source {sources}，預期 {expected}")
    for t in ("compact.started", "compact.finished"):
        n = sum(1 for e in evs if e["type"] == t)
        if n != ROUNDS:
            problems.append(f"{t} 有 {n} 個，預期 {ROUNDS}")

    pos = [e["pos"] for e in evs if e.get("pos") is not None]
    if pos != sorted(set(pos)):
        problems.append(f"pos 不是嚴格遞增：{pos}")
    bad = [e["type"] for e in evs if e["type"] == "transcript.degraded" or e["payload"].get("placement") == "fallback"]
    if bad:
        problems.append(f"有降級：{bad}")

    # transcript 逐行比對：每個 thinking／text／tool_use 行恰好一個對應的 Event
    transcript = Path(next(e["payload"]["transcriptPath"] for e in evs if e["type"] == "session.started"))
    lines = [json.loads(l) for l in transcript.read_text().splitlines() if l.strip()]
    want = {}
    for no, v in enumerate(lines, 1):
        content = (v.get("message") or {}).get("content")
        if v.get("type") != "assistant" or not isinstance(content, list) or not content:
            continue
        if (v["message"].get("model") or "").startswith("<"):
            continue  # <synthetic>：Claude Code 自己產生的訊息
        kind = {"thinking": "assistant.thinking", "text": "assistant.text", "tool_use": "tool.placed"}.get(content[0].get("type"))
        if kind:
            want[(kind, no)] = 0
    got = {}
    for e in evs:
        line = e.get("line") or {}
        if e["type"] in ("assistant.thinking", "assistant.text", "tool.placed") and line.get("file") == "main":
            got[(e["type"], line["no"])] = got.get((e["type"], line["no"]), 0) + 1
    missing = sorted(k for k in want if k not in got)
    twice = sorted(k for k, n in got.items() if n > 1)
    extra = sorted(k for k in got if k not in want)
    if missing:
        problems.append(f"transcript 有、collector 沒有：{missing}")
    if twice:
        problems.append(f"同一行出現兩次：{twice}")
    if extra:
        problems.append(f"collector 有、transcript 沒有：{extra}")

    # 串流狀態行：API 的串流（snapshot／載入更多同一套）裡有每次 resume／compact
    with urllib.request.urlopen(f"http://{web}/api/sessions/{sid}/events?limit=1000") as r:
        stream = json.load(r)["events"]
    rows = [e["payload"]["source"] for e in stream if e["type"] == "session.started" and e["payload"]["source"] != "startup"]
    if rows != [s for s in expected if s != "startup"]:
        problems.append(f"串流裡的 resume／compact 狀態行 {rows}")

    print(f"transcript {len(lines)} 行，比對 {len(want)} 個 thinking／text／tool_use 行；collector {len(evs)} 個 Event")
    print(f"session.started：{sources}")
    if problems:
        print("失敗：\n  " + "\n  ".join(problems))
        sys.exit(1)
    print(f"通過：resume {ROUNDS} 次、compact {ROUNDS} 次，transcript 每行恰好一次、串流只 append、狀態行齊全")


if __name__ == "__main__":
    main()
