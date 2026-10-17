#!/usr/bin/env python3
"""resume／compact 接回的真實操作驗收（#651、#688）：同一個 session 用 `claude -p` 開始，再 resume 三次、compact 三次，
和 Claude Code 寫的 transcript 逐行比對 collector 的 JSONL。

順序：startup → (resume 執行 Bash → resume 執行 /compact) × 3。每次 resume 都讓 Claude 呼叫 Bash，
compact 之後的行也要讀到。檢查：
- session.started 的 source 依序是 startup、resume、resume、compact、resume、resume、compact…；compact.started／finished 各三次；
- transcript 裡每一行 assistant 的 thinking／text 恰好一個對應的 Event、每個 tool_use 恰好一個 tool.placed（stream），
  沒有重複、沒有缺漏；pos 嚴格遞增；沒有降級；
- `/api/sessions/<id>/events` 的串流（依 beforePos 分頁讀完）裡有 resume 與 compact 的狀態行（session.started），次數與上面相同。

不碰正在跑的 collector（4800）與 <repo>/.coding-lab（#688）：在 target/tmp/e2e-lifecycle/ 建獨立的資料目錄、
複製一份 hook 指到空閒埠的 plugin，由 ensure 用剛建好的 release 二進位拉起專屬的 collector，跑完就關掉。
會實際呼叫模型（少量 token）。

用法：tests/scripts/lifecycle.sh（先建 release 再執行本檔）
"""

import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.request
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BIN = ROOT / "target" / "release" / ("coding-lab.exe" if os.name == "nt" else "coding-lab")
WORK = ROOT / "target" / "tmp" / "e2e-lifecycle"
HOME = WORK / "home"
PLUGIN = WORK / "plugin"
ROUNDS = 3
MORE_LIMIT = 500
# 一輪 claude -p 之間的空檔遠短於這個時間；中間有 session 開著時 collector 也不會閒置結束
IDLE_EXIT_SECONDS = 120


def git_init(path):
    """git init 一個獨立的 repo。清掉 GIT_*：pre-commit hook 裡跑時繼承的 GIT_DIR 會讓它去重新初始化外面那個 repo（#703）。"""
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    subprocess.run(["git", "init", "-q", str(path)], check=True, env=env)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def config_toml(hook, web):
    """測試用 collector 的設定。路徑一律寫成 /：Windows 的 E:\\VS\\… 在 TOML 雙引號字串裡是不合法的跳脫（#714）。"""
    return (
        f'[hook]\nbind = "127.0.0.1:{hook}"\n[web]\nbind = "127.0.0.1:{web}"\n'
        # 網頁預設找二進位旁的 web/（#656），target/release/ 底下沒有
        f'root = "{(ROOT / "web").as_posix()}"\n'
        # 腳本被 SIGKILL、沒跑到 stop_collector 時，collector 閒置一陣子會自己結束（#688）
        f"[collector]\nidle_exit_seconds = {IDLE_EXIT_SECONDS}\n"
    )


def setup():
    """獨立的資料目錄與埠；plugin 的 hook url 改指到這個埠。回傳 (hook 埠, web 埠)。
    上一次被中斷的腳本可能留下 collector：先依它的 collector.json 停掉，再清資料目錄（清掉就找不到它了）。"""
    stop_collector()
    shutil.rmtree(HOME, ignore_errors=True)
    shutil.rmtree(PLUGIN, ignore_errors=True)
    HOME.mkdir(parents=True)
    hook, web = free_port(), free_port()
    (HOME / "config.toml").write_text(config_toml(hook, web), encoding="utf-8")
    shutil.copytree(ROOT / "plugin", PLUGIN)
    hooks = PLUGIN / "hooks" / "hooks.json"
    hooks.write_text(hooks.read_text(encoding="utf-8").replace("127.0.0.1:4800", f"127.0.0.1:{hook}"), encoding="utf-8")
    if not (WORK / ".git").exists():  # 自己的 git root，避免讀到上層的 .claude/settings.local.json
        git_init(WORK)
    return hook, web


def alive(pid):
    if os.name == "nt":
        return _alive_windows(pid)
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _alive_windows(pid):
    """Windows 沒有 kill(pid, 0)（對不存在的 pid 丟 WinError 87，#711）：開 handle 等 0 毫秒，逾時表示還在跑。"""
    import ctypes

    k32 = ctypes.windll.kernel32
    synchronize, wait_timeout = 0x0010_0000, 0x102
    h = k32.OpenProcess(synchronize, False, pid)
    if not h:
        return k32.GetLastError() == 5  # ERROR_ACCESS_DENIED：在，只是開不了
    try:
        return k32.WaitForSingleObject(h, 0) == wait_timeout
    finally:
        k32.CloseHandle(h)


def stop_collector():
    """停掉這個資料目錄的 collector：先 `coding-lab stop`（#700，三平台都通、會 flush 並刪 collector.json），
    沒有二進位或它沒停時才送 SIGTERM，5 秒內沒結束就 SIGKILL（Windows 兩者都是 TerminateProcess）。"""
    info = HOME / "collector.json"
    if not info.exists():
        return
    try:
        meta = json.loads(info.read_text(encoding="utf-8"))
        pid = meta["pid"]
    except (ValueError, KeyError):
        return
    # stop 子命令依 collector.json 的 hook 位址找 collector；沒有 hook 的（測試造的）不送，免得打到 4800 的正式 collector
    if BIN.exists() and meta.get("hook"):
        env = dict(os.environ, CODING_LAB_HOME=str(HOME))
        try:
            subprocess.run([str(BIN), "stop"], env=env, timeout=30, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except subprocess.TimeoutExpired:
            pass
        if not alive(pid):
            return
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:  # ProcessLookupError；Windows 是 WinError 87
        return
    for _ in range(50):
        if not alive(pid):
            return
        time.sleep(0.1)
    try:
        os.kill(pid, getattr(signal, "SIGKILL", signal.SIGTERM))
    except OSError:
        pass


def claude(args, prompt):
    env = dict(os.environ, CODING_LAB_BIN=str(BIN), CODING_LAB_HOME=str(HOME))
    r = subprocess.run(
        ["claude", "-p", "--plugin-dir", str(PLUGIN), "--allowedTools", "Bash(echo:*)", *args],
        input=prompt.encode(), env=env, cwd=WORK, capture_output=True, timeout=600,
    )
    print(f"  claude {' '.join(args[:1])} {prompt[:30]!r} → exit {r.returncode}：{r.stdout.decode().strip()[:60]!r}")
    if r.returncode != 0:
        print(r.stderr.decode())
    return r.returncode


def compare(lines, evs):
    """transcript 的行與 collector 的 Event 逐行比對：回傳 (want, missing, twice, extra)。
    want 是應該有 Event 的 (type, 行號)；missing／twice／extra 是缺的、重複的、多出來的。
    tool_use 行在 hook 先到時是 tool.placed，transcript 先到時是帶 pos 的 tool.called，兩種都算放進了串流。
    `<synthetic>` 的行（Claude Code 自己寫的「No response requested.」、用量上限訊息）merge 一樣會發 Event，兩邊都算。"""
    want = {}
    for no, v in enumerate(lines, 1):
        content = (v.get("message") or {}).get("content")
        if v.get("type") != "assistant" or not isinstance(content, list) or not content:
            continue
        kind = {"thinking": "assistant.thinking", "text": "assistant.text", "tool_use": "tool.placed"}.get(content[0].get("type"))
        if kind:
            want[(kind, no)] = 0
    got = {}
    for e in evs:
        line = e.get("line") or {}
        kind = e["type"]
        # transcript 先到（hook 沒來）時，tool_use 直接變成帶 pos 的 tool.called，不會有 tool.placed（contracts.md §3）
        if kind == "tool.called" and e.get("pos") is not None:
            kind = "tool.placed"
        if kind in ("assistant.thinking", "assistant.text", "tool.placed") and line.get("file") == "main":
            got[(kind, line["no"])] = got.get((kind, line["no"]), 0) + 1
    missing = sorted(k for k in want if k not in got)
    twice = sorted(k for k, n in got.items() if n > 1)
    extra = sorted(k for k in got if k not in want)
    return want, missing, twice, extra


def events_of(sid):
    p = HOME / "events" / f"{sid}.jsonl"
    return [json.loads(l) for l in p.read_text(encoding="utf-8").splitlines()] if p.exists() else []


def wait_settled(sid, runs):
    """每次 claude -p 都有一個 SessionEnd；全部到了、而且 JSONL 連續兩次取樣（間隔大於 tail 的 500 ms 輪詢）都沒變才算完。"""
    last, stable = -1, 0
    deadline = time.time() + 30
    while time.time() < deadline:
        evs = events_of(sid)
        ended = sum(1 for e in evs if e["type"] == "session.ended")
        stable = stable + 1 if len(evs) == last else 0
        if ended >= runs and stable >= 2:
            return evs
        last = len(evs)
        time.sleep(0.6)
    return events_of(sid)


def stream_of(web, sid):
    """依 beforePos 往前分頁，讀完整條串流（API 一次最多 500 個 pos）。"""
    out, seen, before = [], set(), None
    while True:
        q = f"limit={MORE_LIMIT}" + (f"&beforePos={before}" if before is not None else "")
        with urllib.request.urlopen(f"http://{web}/api/sessions/{sid}/events?{q}") as r:
            page = json.load(r)["events"]
        for e in page:
            if e["seq"] not in seen:
                seen.add(e["seq"])
                out.append(e)
        pos = [e["pos"] for e in page if e.get("pos") is not None]
        if not pos:
            return out
        before = min(pos)


def main():
    if not BIN.exists():
        sys.exit(f"找不到 {BIN}：請用 tests/scripts/lifecycle.sh 執行（會先建 release）")
    _, web_port = setup()
    web = f"127.0.0.1:{web_port}"
    try:
        run(web)
    finally:
        stop_collector()


def run(web):
    sid = str(uuid.uuid4())
    print(f"session {sid}（資料目錄 {HOME}）")
    codes = [claude(["--session-id", sid], "用 Bash 執行 echo start，然後只回答 OK")]
    expected = ["startup"]
    for i in range(1, ROUNDS + 1):
        codes.append(claude(["--resume", sid], f"用 Bash 執行 echo resume-{i}，然後只回答 OK"))
        expected.append("resume")
        codes.append(claude(["--resume", sid], "/compact"))
        expected += ["resume", "compact"]

    evs = wait_settled(sid, len(codes))
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

    # transcript 逐行比對：每個 thinking／text／tool_use 行恰好一個對應的 Event。
    # 行號只在同一個檔內有意義：resume／compact 實測沿用同一個檔，換檔就要改用 (檔, 行號) 比對
    paths = {e["payload"].get("transcriptPath") for e in evs if e["type"] == "session.started"} - {None}
    lines = []
    if len(paths) != 1:
        problems.append(f"session.started 的 transcriptPath 不是剛好一個：{sorted(paths)}")
    else:
        lines = [json.loads(l) for l in Path(paths.pop()).read_text(encoding="utf-8").splitlines() if l.strip()]
    want, missing, twice, extra = compare(lines, evs)
    if missing:
        problems.append(f"transcript 有、collector 沒有：{missing}")
    if twice:
        problems.append(f"同一行出現兩次：{twice}")
    if extra:
        problems.append(f"collector 有、transcript 沒有：{extra}")

    # 串流狀態行：API 的串流（snapshot／載入更多同一套）裡有每次 resume／compact
    stream = sorted(stream_of(web, sid), key=lambda e: e["seq"])
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
