#!/usr/bin/env python3
"""P1 端到端驗收（#617）：同時跑兩個 Claude Code（裝 plugin），從 WebSocket 檢查合流與頻道。

1. 開三條 WS：`*`、`session:<A>`、`session:<B>`。
2. 用 `claude -p --plugin-dir plugin --session-id` 同時跑兩個 session，各執行幾次 Bash。
3. 等兩個 session 都送出 session.ended，檢查：
   - `*` 收到兩個 session 的事件，而且交錯出現（真的是合流）；
   - 每個 session 自己的 seq 嚴格遞增、pos 嚴格遞增（串流只 append），session.started 最先、session.ended 最後；
   - 每張卡片的 tool.placed 都在 tool.called 之後，沒有降級；
   - `session:<id>` 頻道只含自己的事件，而且和 `*` 裡那個 session 的事件完全相同。

hook 的 url 寫死 127.0.0.1:4800，所以用的是正在跑的 collector（例如主工作樹的自我追蹤 collector）；
web 埠從它的 collector.json 讀。只用標準函式庫（主機沒有 Node／wscat）。

用法：tests/e2e/two_sessions.py [CODING_LAB_HOME，預設 <repo>/.coding-lab]
"""

import base64
import json
import os
import socket
import struct
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BIN = ROOT / "target" / "release" / ("coding-lab.exe" if os.name == "nt" else "coding-lab")
WORK = ROOT / "target" / "tmp" / "e2e-two-sessions"


class WS:
    """最小的 WebSocket 用戶端（RFC 6455，只用文字訊息）。不帶 Origin，就和 wscat 一樣。"""

    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=30)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall(
            f"GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode()
        )
        head = b""
        while b"\r\n\r\n" not in head:
            head += self.s.recv(1)
        assert head.startswith(b"HTTP/1.1 101"), head
        self.lock = threading.Lock()

    def send(self, obj):
        data = json.dumps(obj).encode()
        mask = os.urandom(4)
        n = len(data)
        hdr = bytes([0x81]) + (bytes([0x80 | n]) if n < 126 else bytes([0x80 | 126]) + struct.pack(">H", n))
        with self.lock:
            self.s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def _read(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.s.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("WS 關閉")
            buf += chunk
        return buf

    def recv(self):
        msg = b""
        while True:
            b0, b1 = self._read(2)
            n = b1 & 0x7F
            if n == 126:
                n = struct.unpack(">H", self._read(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self._read(8))[0]
            payload = self._read(n)
            op = b0 & 0x0F
            if op == 9:  # ping → pong
                with self.lock:
                    self.s.sendall(bytes([0x8A, 0x80 | len(payload)]) + b"\0\0\0\0" + payload)
                continue
            if op == 8:
                raise ConnectionError("WS 關閉")
            msg += payload
            if b0 & 0x80:
                return json.loads(msg)


def listen(port, channel, out, stop):
    ws = WS(port)
    ws.send({"op": "hello", "protocol": 1})
    ws.send({"op": "subscribe", "channel": channel})
    while not stop.is_set():
        try:
            m = ws.recv()
        except (socket.timeout, ConnectionError):
            break
        if m.get("t") == "frame" and m.get("ch") == channel:
            out.extend(m.get("events", []))


PROMPT = "依序用 Bash 執行 echo {tag}-1、echo {tag}-2、echo {tag}-3（每次一個指令，分三次呼叫），最後只回答 OK"


def run_claude(sid, tag, home):
    env = dict(os.environ, CODING_LAB_BIN=str(BIN), CODING_LAB_HOME=str(home))
    return subprocess.run(
        ["claude", "-p", "--plugin-dir", str(ROOT / "plugin"), "--session-id", sid, "--allowedTools", "Bash(echo:*)"],
        input=PROMPT.format(tag=tag).encode(), env=env, cwd=WORK, capture_output=True, timeout=300,
    )


def check_session(name, evs):
    problems = []
    seqs = [e["seq"] for e in evs]
    if seqs != sorted(set(seqs)):
        problems.append(f"{name}: seq 不是嚴格遞增")
    pos = [e["pos"] for e in evs if e.get("pos") is not None]
    if pos != sorted(set(pos)):
        problems.append(f"{name}: pos 不是嚴格遞增（往前插入）{pos}")
    types = [e["type"] for e in evs]
    if not types or types[0] != "session.started" or types[-1] != "session.ended":
        problems.append(f"{name}: 頭尾不是 session.started／session.ended：{types[:2]}…{types[-2:]}")
    called = {}
    for i, e in enumerate(evs):
        if e["type"] == "tool.called":
            called[e["payload"]["toolUseId"]] = i
        if e["type"] == "tool.placed":
            tid = e["payload"]["toolUseId"]
            if e["payload"]["placement"] != "stream":
                problems.append(f"{name}: {tid} placement={e['payload']['placement']}")
            if called.get(tid, i + 1) > i:
                problems.append(f"{name}: {tid} 的 tool.placed 在 tool.called 之前")
    return problems


def main():
    home = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / ".coding-lab"
    info = json.loads((home / "collector.json").read_text(encoding="utf-8"))
    web_port = int(info["web"].rsplit(":", 1)[1])
    WORK.mkdir(parents=True, exist_ok=True)
    if not (WORK / ".git").exists():  # 自己的 git root，避免讀到上層的 .claude/settings.local.json
        env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}  # 同 lifecycle.git_init（#703）
        subprocess.run(["git", "init", "-q", str(WORK)], check=True, env=env)

    a, b = str(uuid.uuid4()), str(uuid.uuid4())
    all_events, ch_a, ch_b = [], [], []
    stop = threading.Event()
    listeners = [
        threading.Thread(target=listen, args=(web_port, ch, out, stop), daemon=True)
        for ch, out in (("*", all_events), (f"session:{a}", ch_a), (f"session:{b}", ch_b))
    ]
    for t in listeners:
        t.start()
    time.sleep(0.5)

    print(f"session A {a}\nsession B {b}\n同時執行兩個 claude -p …")
    results = {}
    runners = [
        threading.Thread(target=lambda s=s, tag=tag: results.update({tag: run_claude(s, tag, home)}))
        for s, tag in ((a, "A"), (b, "B"))
    ]
    for t in runners:
        t.start()
    for t in runners:
        t.join()
    for tag, r in results.items():
        print(f"claude {tag}：exit {r.returncode}、回答 {r.stdout.decode().strip()!r}、stderr {r.stderr.decode().strip()!r}")

    deadline = time.time() + 15
    ended = lambda: {e["session"] for e in all_events if e["type"] == "session.ended"} >= {a, b}  # noqa: E731
    while not ended() and time.time() < deadline:
        time.sleep(0.2)
    time.sleep(0.5)
    stop.set()

    ev_a = [e for e in all_events if e["session"] == a]
    ev_b = [e for e in all_events if e["session"] == b]
    order = "".join("A" if e["session"] == a else "B" for e in all_events if e["session"] in (a, b))
    switches = sum(1 for x, y in zip(order, order[1:]) if x != y)
    print(f"`*` 收到：A {len(ev_a)} 個、B {len(ev_b)} 個 Event；交錯順序 {order}（切換 {switches} 次）")
    print(f"session:A 收到 {len(ch_a)} 個、session:B 收到 {len(ch_b)} 個")

    problems = check_session("A", ev_a) + check_session("B", ev_b)
    if switches < 2:
        problems.append("`*` 裡兩個 session 沒有交錯，看不出合流")
    for name, ch, own, sid in (("A", ch_a, ev_a, a), ("B", ch_b, ev_b, b)):
        others = {e["session"] for e in ch} - {sid}
        if others:
            problems.append(f"session:{name} 頻道混進其他 session：{others}")
        if [e["seq"] for e in ch] != [e["seq"] for e in own]:
            problems.append(f"session:{name} 頻道和 `*` 裡的 {name} 不一致")
    if any(r.returncode != 0 for r in results.values()):
        problems.append("有 claude 結束碼不是 0")

    if problems:
        print("失敗：\n  " + "\n  ".join(problems))
        sys.exit(1)
    print("通過：合流順序正確、各 session 頻道只含自己的事件")


if __name__ == "__main__":
    main()
