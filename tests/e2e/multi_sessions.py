#!/usr/bin/env python3
"""多 session 長時間的真實驗收（#658，PLAN「完成標準」）：N 個裝 plugin 的 Claude Code 同時跑 D 分鐘。

每個 session 在自己的 git 目錄裡輪流做 Bash、Edit、Write 與純文字回答（每分鐘左右一次 `claude -p --resume`），
跑到一半用 SIGKILL 砍掉 collector（下一個 SessionStart 的 ensure 會把它拉起來，中間的 hook 都送不到）。檢查：
- 每個 session 的 transcript 每一行 thinking／text／tool_use 恰好一個 Event（停機期間寫的行重啟後也要補回來，#684）；
- 同 session 的 pos 嚴格遞增，而且依 transcript 行號的順序（thinking／text 不會排在它導致的動作之後）；
- 每次 claude 都 exit 0（collector 掛掉時 Claude Code 照常運作），停機前後的耗時差不多；
- Edit 的行號：每次呼叫前後各存一份 notes.txt，第一個改動的行要等於 file.edit hunk 的位置（抽查 EDIT_SAMPLES 筆）；
- collector 的 CPU：每 5 秒取樣 /proc，平均與最高（單核 %）。

和 lifecycle.py 一樣不碰 4800 與 <repo>/.coding-lab：在 target/tmp/e2e-multi/ 建獨立的資料目錄與 plugin 副本。
會實際呼叫模型（預設 haiku；N=16、D=30 約 500 次 claude -p）。

用法：tests/scripts/multi.sh [N=16] [D=30]
"""

import json
import os
import random
import shutil
import signal
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lifecycle as L  # noqa: E402  重用 setup、stop_collector、compare、events_of

L.WORK = L.ROOT / "target" / "tmp" / "e2e-multi"
L.HOME = L.WORK / "home"
L.PLUGIN = L.WORK / "plugin"
L.IDLE_EXIT_SECONDS = 300

MODEL = os.environ.get("E2E_MODEL", "haiku")
PACE_SECONDS = 60
EDIT_SAMPLES = 20
NOTES_LINES = 40

lock = threading.Lock()
calls = []  # (sid, 開始秒, 耗時, exit code, 是否在停機期間之後)
edits = []  # (sid, 開始 ms, 結束 ms, 第一個改動的行號)
cpu = []  # (時間, 單核 %)


def log(msg):
    with lock:
        print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def claude(cwd, sid, first, prompt):
    env = dict(os.environ, CODING_LAB_BIN=str(L.BIN), CODING_LAB_HOME=str(L.HOME))
    args = ["--session-id", sid] if first else ["--resume", sid]
    cmd = ["claude", "-p", "--model", MODEL, "--plugin-dir", str(L.PLUGIN),
           "--allowedTools", "Bash(echo:*)", "Edit", "Write", "Read", *args]
    try:
        r = subprocess.run(cmd, input=prompt.encode(), env=env, cwd=cwd, capture_output=True, timeout=600)
        return r.returncode
    except subprocess.TimeoutExpired:
        return -1


def first_changed_line(before, after):
    a, b = before.splitlines(), after.splitlines()
    for i, (x, y) in enumerate(zip(a, b)):
        if x != y:
            return i + 1
    return min(len(a), len(b)) + 1 if len(a) != len(b) else None


def hunk_changed_line(h):
    """hunk 裡第一個真的改掉的行號。hunk 照 old_string／new_string 原樣標 −／+，模型常把沒改的相鄰行也放進去，
    所以跳過開頭的 context，再跳過 − 與 + 開頭相同的行。"""
    lines = h["lines"]
    ctx = next((i for i, l in enumerate(lines) if not l.startswith(" ")), len(lines))
    minus = [l[1:] for l in lines[ctx:] if l.startswith("-")]
    plus = [l[1:] for l in lines[ctx:] if l.startswith("+")]
    same = next((i for i, (a, b) in enumerate(zip(minus, plus)) if a != b), min(len(minus), len(plus)))
    return h["start"] + ctx + same


def workdir(i):
    """每次都從空目錄開始：上一次留下的 out-*.txt 會讓 Write 在 PreToolUse 之前就被擋下（要先 Read）。"""
    cwd = L.WORK / f"s{i:02d}"
    shutil.rmtree(cwd, ignore_errors=True)
    cwd.mkdir(parents=True)
    L.git_init(cwd)
    (cwd / "notes.txt").write_text("".join(f"line {n}\n" for n in range(1, NOTES_LINES + 1)), encoding="utf-8")
    return cwd


def session(i, deadline, killed_at):
    sid = str(uuid.uuid4())
    cwd = workdir(i)
    notes = cwd / "notes.txt"
    k = 0
    while time.time() < deadline:
        t0 = time.time()
        kind = k % 4
        if kind == 0:
            prompt = f"用 Bash 執行 echo s{i}-round-{k}，然後只回答 OK"
        elif kind == 1:
            line = random.randint(2, NOTES_LINES - 1)
            prompt = f"用 Edit 工具把 notes.txt 裡內容是「{notes.read_text(encoding="utf-8").splitlines()[line - 1]}」的那一行改成「edited {k}」，然後只回答 OK"
        elif kind == 2:
            prompt = f"用 Write 工具建立 out-{k}.txt，內容是三行：a、b、c。然後只回答 OK"
        else:
            prompt = "不要用任何工具，用一句話說明你剛才做了什麼"
        before = notes.read_text(encoding="utf-8")
        start_ms = int(time.time() * 1000)
        code = claude(cwd, sid, k == 0, prompt)
        end_ms = int(time.time() * 1000)
        took = time.time() - t0
        after = notes.read_text(encoding="utf-8")
        with lock:
            calls.append((sid, t0, took, code, killed_at[0] is not None and t0 > killed_at[0]))
            if kind == 1 and before != after:
                edits.append((sid, start_ms, end_ms, first_changed_line(before, after)))
        if code != 0:
            log(f"s{i:02d} 第 {k} 次 exit {code}")
        k += 1
        time.sleep(max(0.0, PACE_SECONDS - took) * random.uniform(0.8, 1.0))
    return sid, k


def collector_pid():
    try:
        return json.loads((L.HOME / "collector.json").read_text(encoding="utf-8"))["pid"]
    except (OSError, ValueError, KeyError):
        return None


PROC = Path("/proc")


def cpu_seconds(pid):
    """collector 用掉的 CPU 秒數（user＋system）。Linux 讀 /proc；Windows 用 GetProcessTimes（沒有 /proc，#717）；
    取不到回 None，報告標「無 CPU 數據」而不是讓腳本失敗。"""
    if os.name == "nt":
        return _cpu_seconds_windows(pid)
    try:
        f = (PROC / str(pid) / "stat").read_text(encoding="utf-8").rsplit(")", 1)[1].split()
        return (int(f[11]) + int(f[12])) / os.sysconf("SC_CLK_TCK")
    except (OSError, IndexError, ValueError):
        return None


def _cpu_seconds_windows(pid):
    import ctypes
    from ctypes import wintypes

    k32 = ctypes.windll.kernel32
    query_limited = 0x1000
    h = k32.OpenProcess(query_limited, False, pid)
    if not h:
        return None
    try:
        t = [wintypes.FILETIME() for _ in range(4)]  # creation, exit, kernel, user
        if not k32.GetProcessTimes(h, *(ctypes.byref(x) for x in t)):
            return None
        ticks = sum(((x.dwHighDateTime << 32) | x.dwLowDateTime) for x in t[2:])
        return ticks / 10_000_000  # 100 ns 一格
    finally:
        k32.CloseHandle(h)


def hard_kill(pid):
    """強制結束（模擬 collector 當掉）：Windows 沒有 SIGKILL，os.kill 送 SIGTERM 就是 TerminateProcess（#717）；
    已結束的 pid 不丟例外。"""
    try:
        os.kill(pid, getattr(signal, "SIGKILL", signal.SIGTERM))
    except OSError:
        return
    for _ in range(50):
        if not L.alive(pid):
            return
        time.sleep(0.1)


def sampler(stop):
    last = {}
    while not stop.is_set():
        pid = collector_pid()
        now = time.time()
        c = cpu_seconds(pid) if pid else None
        if c is not None and pid in last:
            t, prev = last[pid]
            cpu.append((now, 100.0 * (c - prev) / (now - t)))
        if c is not None:
            last[pid] = (now, c)
        stop.wait(5)


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 16
    minutes = float(sys.argv[2]) if len(sys.argv) > 2 else 30
    if not L.BIN.exists():
        sys.exit(f"找不到 {L.BIN}，先 cargo build --release -p coding-lab")
    L.setup()
    log(f"{n} 個 session、{minutes} 分鐘、模型 {MODEL}；資料目錄 {L.HOME}")
    deadline = time.time() + minutes * 60
    killed_at = [None]
    stop = threading.Event()
    threading.Thread(target=sampler, args=(stop,), daemon=True).start()
    results = [None] * n

    def run(i):
        time.sleep(i * 2)  # 錯開啟動
        results[i] = session(i, deadline, killed_at)

    threads = [threading.Thread(target=run, args=(i,)) for i in range(n)]
    for t in threads:
        t.start()

    # 跑到一半砍掉 collector
    time.sleep(minutes * 30)
    pid = collector_pid()
    if pid:
        hard_kill(pid)
        killed_at[0] = time.time()
        log(f"強制結束 collector（pid {pid}）；下一個 SessionStart 會由 ensure 拉起新的")
    for t in threads:
        t.join()
    stop.set()
    time.sleep(3)
    report(results, killed_at[0])
    L.stop_collector()


def report(results, killed_at):
    ok = True
    total_lines = total_events = 0
    order_bad = []
    for sid, runs in results:
        evs = L.wait_settled(sid, runs)
        started = next((e for e in evs if e["type"] == "session.started"), None)
        path = started and started["payload"].get("transcriptPath")
        lines = [json.loads(x) for x in Path(path).read_text(encoding="utf-8").splitlines() if x.strip()] if path else []
        want, missing, twice, extra = L.compare(lines, evs)
        total_lines += len(want)
        total_events += len(evs)
        pos = [e["pos"] for e in evs if e.get("pos") is not None]
        by_line = sorted((e["line"]["no"], e["pos"]) for e in evs
                         if e.get("pos") is not None and (e.get("line") or {}).get("file") == "main")
        if any(b[1] <= a[1] for a, b in zip(by_line, by_line[1:])):
            order_bad.append(sid[:8])
        if missing or twice or extra or any(b <= a for a, b in zip(pos, pos[1:])):
            ok = False
            log(f"✗ {sid[:8]}：缺 {missing[:5]}、重複 {twice[:5]}、多 {extra[:5]}")
    fallback = 0
    for sid, _ in results:
        fallback += sum(1 for e in L.events_of(sid)
                        if e["type"] == "tool.placed" and e["payload"].get("placement") == "fallback")
    log(f"逐行比對：{len(results)} 個 session、{total_lines} 行、{total_events} 個 Event；降級放置 {fallback}")
    if order_bad:
        ok = False
        log(f"✗ 依行號排序的 pos 不遞增：{order_bad}")

    codes = [c for (_, _, _, c, _) in calls]
    bad = [c for c in codes if c != 0]
    before = [d for (_, _, d, _, after) in calls if not after]
    after = [d for (_, _, d, _, after) in calls if after]
    avg = lambda xs: sum(xs) / len(xs) if xs else 0  # noqa: E731
    log(f"claude -p：{len(codes)} 次，非 0 結束 {len(bad)}；平均耗時 停機前 {avg(before):.1f}s、之後 {avg(after):.1f}s")
    if bad:
        ok = False

    # Edit 行號抽查
    checked = mismatch = 0
    file_edits = {}
    for sid, _ in results:
        file_edits[sid] = [e for e in L.events_of(sid) if e["type"] == "file.edit"
                           and e["payload"].get("path", "").endswith("notes.txt") and e["payload"].get("hunk")]
    for sid, start_ms, end_ms, line in random.sample(edits, min(EDIT_SAMPLES, len(edits))):
        hits = [e for e in file_edits[sid] if start_ms <= e["ts_seen"] <= end_ms]
        if not hits or line is None:
            continue
        h = hits[0]["payload"]["hunk"]
        checked += 1
        if hunk_changed_line(h) != line:
            mismatch += 1
            log(f"✗ Edit 行號 {sid[:8]}：hunk 指向第 {hunk_changed_line(h)} 行（start {h['start']}），實際第 {line} 行")
    log(f"Edit 行號抽查：{checked} 筆，不符 {mismatch}")
    if mismatch or checked < min(EDIT_SAMPLES, len(edits)) // 2:
        ok = False

    if cpu:
        vals = sorted(v for _, v in cpu)
        log(f"collector CPU（單核 %）：平均 {avg(vals):.1f}、p95 {vals[max(0, int(len(vals) * 0.95) - 1)]:.1f}、最高 {vals[-1]:.1f}（{len(vals)} 個樣本，每 5 秒）")
        if avg(vals) >= 30:  # PLAN：16 個 session 跑 30 分鐘 CPU < 一核的 30%
            ok = False
    else:
        log("collector CPU：無數據（這個平台取不到，或 collector 沒跑滿一個取樣間隔）")
    log("通過" if ok else "未通過")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
