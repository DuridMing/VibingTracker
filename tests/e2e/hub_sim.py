#!/usr/bin/env python3
"""P6 hub 的單機驗收（#663、#715 的程式碼部分）：兩個 collector 在不同埠與資料目錄當兩台上游。

1. 上游 alpha：`auth = "always"`，建一把 view 金鑰，hub 用 Bearer 連；上游 beta：local-open、免金鑰（`hub add --no-key`）。
2. `coding-lab hub` 綁隨機本機埠；對兩台各送一組 PreToolUse／PostToolUse（合成 payload，沒有 transcript）。
3. 檢查 hub：`/healthz` 兩台 online、`/api/sessions` 兩個 session 各帶 host；WS `*` 的 welcome.host 是 hub、
   snapshot／frame 的 Event 與 sessions 列帶 host；alpha（view）的 session 按 ack 回 error、beta 的 ack 成功。
4. SIGKILL alpha → 量 hub 多久標 offline（/healthz online=false、sessions 列 offline=true），beta 照常收事件；
   重新拉起 alpha → 量多久回到 online，之後 alpha 的新事件要出現在 hub。

只用標準函式庫，資料目錄在 target/tmp/hub-sim/，不碰 ~/.coding-lab 與 4800／4801。
用法：tests/e2e/hub_sim.py（由 tests/scripts/hub.sh 建置後呼叫）
"""

import http.client
import json
import os
import re
import signal
import socket
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BIN = ROOT / "target" / "release" / ("coding-lab.exe" if os.name == "nt" else "coding-lab")
WORK = ROOT / "target" / "tmp" / "hub-sim"
sys.path.insert(0, str(Path(__file__).resolve().parent))
from two_sessions import WS  # noqa: E402  最小的 WebSocket 用戶端


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def http_get(port, path, headers=None):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    c.request("GET", path, headers=headers or {})
    r = c.getresponse()
    body = r.read()
    c.close()
    try:
        return r.status, json.loads(body)
    except ValueError:
        return r.status, body.decode(errors="replace")


def post_hook(port, payload):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    c.request("POST", "/hook", body=json.dumps(payload).encode(), headers={"Content-Type": "application/json"})
    r = c.getresponse()
    r.read()
    c.close()
    return r.status


class Collector:
    def __init__(self, name, extra_config=""):
        self.name = name
        self.home = WORK / name
        subprocess.run(["rm", "-rf", str(self.home)], check=True)
        self.home.mkdir(parents=True)
        self.hook_port, self.web_port = free_port(), free_port()
        (self.home / "config.toml").write_text(
            f'[hook]\nbind = "127.0.0.1:{self.hook_port}"\n'
            f'[web]\nbind = "127.0.0.1:{self.web_port}"\n{extra_config}'
            "[collector]\nidle_exit_seconds = 0\n"
        )
        self.proc = None

    def env(self):
        return dict(os.environ, CODING_LAB_HOME=str(self.home))

    def run(self, *args, input=None):
        return subprocess.run([str(BIN), *args], env=self.env(), input=input, capture_output=True, text=True, timeout=30)

    def start(self):
        log = open(self.home / "collector.log", "ab")
        self.proc = subprocess.Popen([str(BIN), "serve"], env=self.env(), stdout=subprocess.DEVNULL, stderr=log)
        deadline = time.time() + 10
        while True:
            try:
                if http_get(self.web_port, "/healthz")[0] == 200:
                    return self
            except OSError:
                pass
            if time.time() > deadline:
                raise SystemExit(f"{self.name} 10 秒內沒有起來，見 {self.home / 'collector.log'}")
            time.sleep(0.05)

    def kill(self):
        self.proc.send_signal(signal.SIGKILL)
        self.proc.wait(timeout=10)
        # 單例鎖與 collector.json 由下一次啟動自己處理
        self.proc = None

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()


def tool_pair(sid, cwd, cmd):
    tid = "toolu_" + uuid.uuid4().hex[:20]
    base = {"session_id": sid, "cwd": cwd, "permission_mode": "default", "tool_name": "Bash",
            "tool_input": {"command": cmd, "description": cmd}, "tool_use_id": tid}
    pre = dict(base, hook_event_name="PreToolUse")
    post = dict(base, hook_event_name="PostToolUse", tool_response={"stdout": cmd + "\n", "stderr": "", "interrupted": False})
    return pre, post


def wait_until(pred, timeout, what):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            if pred():
                return time.time() - t0
        except OSError:
            pass
        time.sleep(0.1)
    raise SystemExit(f"等 {timeout} 秒沒等到：{what}")


class HubListener:
    """訂 hub 的 `*`，把 welcome／snapshot／frame／error 收進清單。"""

    def __init__(self, port):
        self.ws = WS(port)
        self.msgs = []
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.ws.s.settimeout(1)
        self.ws.send({"op": "hello", "protocol": 1})
        self.ws.send({"op": "subscribe", "channel": "*"})
        threading.Thread(target=self._loop, daemon=True).start()

    def _loop(self):
        while not self.stop.is_set():
            try:
                m = self.ws.recv()
            except socket.timeout:
                continue
            except ConnectionError:
                break
            with self.lock:
                self.msgs.append(m)

    def take(self):
        with self.lock:
            out, self.msgs = self.msgs, []
        return out

    def events(self, msgs):
        out = []
        for m in msgs:
            if m.get("t") in ("snapshot", "frame"):
                out.extend(m.get("events", []) + m.get("stream", []))
        return out

    def session_rows(self, msgs):
        rows = {}
        for m in msgs:
            if m.get("t") in ("snapshot", "frame"):
                for r in (m.get("sessions") or m.get("rows", {}).get("sessions") or []):
                    if isinstance(r, dict) and r.get("id"):
                        rows[r["id"]] = r
        return rows


def main():
    if not BIN.exists():
        raise SystemExit(f"先建置：cargo build --release -p coding-lab（找不到 {BIN}）")
    WORK.mkdir(parents=True, exist_ok=True)
    report = []
    ok = True

    def check(cond, msg):
        nonlocal ok
        report.append(("✅" if cond else "❌") + " " + msg)
        ok = ok and bool(cond)

    alpha = Collector("alpha", extra_config='auth = "always"\n')
    beta = Collector("beta")
    created = alpha.run("key", "create", "--name", "hub-sim", "--scope", "view")
    m = re.search(r"clk_[A-Za-z0-9]+_[A-Za-z0-9]+", created.stdout)
    if not m:
        raise SystemExit("alpha 建金鑰失敗：" + created.stdout + created.stderr)
    alpha_key = m.group(0)

    hub = Collector("hub")
    hub_port = free_port()
    (hub.home / "config.toml").write_text(
        f'[hub]\nbind = "127.0.0.1:{hub_port}"\n'
        f'[[hub.upstream]]\nname = "alpha"\nurl = "http://127.0.0.1:{alpha.web_port}"\nkey = "{alpha_key}"\n'
    )
    added = hub.run("hub", "add", "beta", f"http://127.0.0.1:{beta.web_port}", "--no-key")
    check(added.returncode == 0 and "beta" in (hub.home / "config.toml").read_text(), "hub add --no-key 把 beta 寫進 config.toml")

    alpha.start()
    beta.start()
    hub_log = open(hub.home / "hub.log", "ab")
    hub_proc = subprocess.Popen([str(BIN), "hub"], env=hub.env(), stdout=hub_log, stderr=hub_log)
    try:
        t_up = wait_until(lambda: http_get(hub_port, "/healthz")[0] == 200, 10, "hub 起來")
        h = http_get(hub_port, "/healthz")[1]
        check(h.get("hub") is True, f"hub /healthz 標 hub: true（{t_up:.2f} 秒起來）")
        both = wait_until(lambda: all(u["online"] for u in http_get(hub_port, "/healthz")[1]["upstreams"]), 15, "兩台 online")
        check(True, f"兩台上游 online（{both:.2f} 秒）：{[u['name'] for u in h['upstreams']]}")

        listener = HubListener(hub_port)
        time.sleep(0.5)
        welcome = [m for m in listener.take() if m.get("t") == "welcome"]
        check(welcome and welcome[0].get("host") == "hub", f"welcome.host 是 hub：{welcome[0] if welcome else None}")
        check(welcome and welcome[0].get("scope") == "operate", "welcome.scope 是 operate")

        sid_a, sid_b = str(uuid.uuid4()), str(uuid.uuid4())
        cwd_a, cwd_b = str(WORK / "proj-a"), str(WORK / "proj-b")
        for cwd in (cwd_a, cwd_b):
            Path(cwd).mkdir(exist_ok=True)
        for pre, post in [tool_pair(sid_a, cwd_a, "echo alpha-1"), tool_pair(sid_a, cwd_a, "echo alpha-2")]:
            assert post_hook(alpha.hook_port, pre) == 200 and post_hook(alpha.hook_port, post) == 200
        for pre, post in [tool_pair(sid_b, cwd_b, "echo beta-1")]:
            assert post_hook(beta.hook_port, pre) == 200 and post_hook(beta.hook_port, post) == 200

        wait_until(lambda: {s["id"] for s in http_get(hub_port, "/api/sessions")[1]["sessions"]} >= {sid_a, sid_b}, 15, "hub /api/sessions 有兩台的 session")
        rows = {s["id"]: s for s in http_get(hub_port, "/api/sessions")[1]["sessions"]}
        check(rows[sid_a].get("host") == "alpha" and rows[sid_b].get("host") == "beta", f"/api/sessions 每列帶 host：{rows[sid_a].get('host')}, {rows[sid_b].get('host')}")

        time.sleep(1.0)
        msgs = listener.take()
        evs = listener.events(msgs)
        hosts = {e.get("host") for e in evs if e.get("session") in (sid_a, sid_b)}
        check(hosts >= {"alpha", "beta"}, f"WS `*` 的 Event 帶上游名稱當 host：{sorted(h for h in hosts if h)}")
        per = {sid: [e for e in evs if e.get("session") == sid] for sid in (sid_a, sid_b)}
        check(all(per[s] for s in per), f"兩台的事件都進了同一條串流（alpha {len(per[sid_a])} 筆、beta {len(per[sid_b])} 筆）")
        for sid in (sid_a, sid_b):
            seqs = [e["seq"] for e in per[sid]]
            check(seqs == sorted(seqs), f"{sid[:8]} 在 hub 裡 seq 不倒退")

        # ack：alpha 是 view 金鑰 → error；beta 本機 → 成功（沒有 error）
        listener.take()
        listener.ws.send({"op": "ack", "session": sid_a, "kind": "done", "ref": "x"})
        time.sleep(0.8)
        errs = [m for m in listener.take() if m.get("t") == "error"]
        check(errs and "view" in json.dumps(errs, ensure_ascii=False), f"view 金鑰的上游 ack 回 error：{errs[:1]}")
        listener.ws.send({"op": "ack", "session": sid_b, "kind": "done", "ref": "x"})
        time.sleep(0.8)
        errs = [m for m in listener.take() if m.get("t") == "error"]
        check(not errs, "本機（免金鑰）上游的 ack 沒有 error")

        # 轉發：thinking API 與 /api/sessions/<id>/events 轉給擁有 session 的那台
        st, body = http_get(hub_port, f"/api/sessions/{sid_a}/events?limit=10")
        check(st == 200 and all(e.get("host") == "alpha" for e in body.get("events", [])), f"/api/sessions/<alpha 的 id>/events 經 hub 轉發且加 host（{st}）")
        st, _ = http_get(hub_port, f"/api/sessions/{uuid.uuid4()}/events")
        check(st == 404, f"不認得的 session 回 404（{st}）")

        # 斷線：SIGKILL alpha
        listener.take()
        alpha.kill()
        t_off = wait_until(lambda: not [u for u in http_get(hub_port, "/healthz")[1]["upstreams"] if u["name"] == "alpha"][0]["online"], 60, "alpha 標 offline")
        check(True, f"alpha 被 kill 後 hub 在 {t_off:.1f} 秒標 online=false")
        time.sleep(0.5)
        off_rows = listener.session_rows(listener.take())
        check(off_rows.get(sid_a, {}).get("offline") is True, f"alpha 的 sessions 列送 offline: true：{off_rows.get(sid_a, {}).get('offline')}")
        beta_up = [u for u in http_get(hub_port, "/healthz")[1]["upstreams"] if u["name"] == "beta"][0]["online"]
        check(beta_up, "beta 仍 online")
        pre, post = tool_pair(sid_b, cwd_b, "echo beta-while-alpha-down")
        assert post_hook(beta.hook_port, pre) == 200 and post_hook(beta.hook_port, post) == 200
        time.sleep(1.0)
        evs = listener.events(listener.take())
        check(any(e.get("session") == sid_b and e.get("type") == "tool.called" for e in evs), "alpha 離線期間 beta 的事件照常到 hub")
        st, _ = http_get(hub_port, f"/api/sessions/{sid_a}/events?limit=1")
        check(st == 502, f"離線上游的轉發回 502（{st}）")

        # 重新拉起 alpha
        alpha.start()
        t_on = wait_until(lambda: [u for u in http_get(hub_port, "/healthz")[1]["upstreams"] if u["name"] == "alpha"][0]["online"], 60, "alpha 回到 online")
        check(True, f"alpha 重啟後 hub 在 {t_on:.1f} 秒重連")
        time.sleep(0.5)
        back = listener.take()
        rows = listener.session_rows(back)
        check(sid_a in rows and not rows[sid_a].get("offline"), f"重連後重送 snapshot，alpha 的列不再 offline（{rows.get(sid_a, {}).get('offline')}）")
        pre, post = tool_pair(sid_a, cwd_a, "echo alpha-after-reconnect")
        assert post_hook(alpha.hook_port, pre) == 200 and post_hook(alpha.hook_port, post) == 200
        time.sleep(1.0)
        evs = listener.events(listener.take())
        check(any(e.get("session") == sid_a and e.get("host") == "alpha" and e.get("type") == "tool.called" for e in evs), "重連後 alpha 的新事件出現在 hub")
        listener.stop.set()
    finally:
        hub_proc.terminate()
        try:
            hub_proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            hub_proc.kill()
        alpha.stop()
        beta.stop()

    print("\n".join(report))
    print("\nhub 單機模擬：" + ("全部通過" if ok else "有失敗"))
    print(f"log：{hub.home / 'hub.log'}、{alpha.home / 'collector.log'}、{beta.home / 'collector.log'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
