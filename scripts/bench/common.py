"""bench 共用：啟動一個隔離的 collector（自己的資料目錄與埠）、送 hook、讀 /proc 的 CPU。

資料目錄放在 target/tmp/bench-*（專案目錄內，不碰 ~/.coding-lab），用完自動停掉 collector。
"""

import http.client
import json
import os
import socket
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BIN = ROOT / "target" / "release" / "coding-lab"
SAMPLES = ROOT / "samples" / "sessions"
CLK_TCK = os.sysconf("SC_CLK_TCK")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Collector:
    """`coding-lab serve` 跑在 target/tmp/bench-<name>/，hook 與 web 用隨機埠。"""

    def __init__(self, name):
        if not BIN.exists():
            raise SystemExit(f"先建置：cargo build --release -p coding-lab（找不到 {BIN}）")
        self.home = ROOT / "target" / "tmp" / f"bench-{name}"
        subprocess.run(["rm", "-rf", str(self.home)], check=True)
        self.home.mkdir(parents=True)
        self.hook_port, self.web_port = free_port(), free_port()
        (self.home / "config.toml").write_text(
            f'[hook]\nbind = "127.0.0.1:{self.hook_port}"\n'
            f'[web]\nbind = "127.0.0.1:{self.web_port}"\n'
            "[collector]\nidle_exit_seconds = 0\n"
        )

    def __enter__(self):
        env = dict(os.environ, CODING_LAB_HOME=str(self.home))
        log = open(self.home / "collector.log", "ab")
        self.proc = subprocess.Popen([str(BIN), "serve"], env=env, stdout=subprocess.DEVNULL, stderr=log)
        deadline = time.time() + 5
        while not (self.home / "collector.json").exists():
            if time.time() > deadline:
                raise SystemExit("collector 5 秒內沒有起來，見 " + str(self.home / "collector.log"))
            time.sleep(0.02)
        return self

    def __exit__(self, *exc):
        self.proc.terminate()
        self.proc.wait(timeout=10)

    def post_hook(self, payload: bytes):
        """一次一條新連線（和 Claude Code 的 http hook 一樣），回傳 (status, 秒)。"""
        t = time.perf_counter()
        c = http.client.HTTPConnection("127.0.0.1", self.hook_port, timeout=5)
        c.request("POST", "/hook", body=payload, headers={"Content-Type": "application/json"})
        r = c.getresponse()
        r.read()
        c.close()
        return r.status, time.perf_counter() - t

    def healthz(self):
        c = http.client.HTTPConnection("127.0.0.1", self.web_port, timeout=5)
        c.request("GET", "/healthz")
        body = json.loads(c.getresponse().read())
        c.close()
        return body

    def cpu_seconds(self):
        """collector 累計用掉的 CPU 秒數（user＋sys）。"""
        fields = Path(f"/proc/{self.proc.pid}/stat").read_text().rsplit(")", 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / CLK_TCK

    def rss_mb(self):
        for line in Path(f"/proc/{self.proc.pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024
        return 0.0


def read_jsonl(path):
    if not path.exists():
        return []
    return [json.loads(l) for l in path.read_text().splitlines() if l.strip()]


def sample_dirs():
    if not SAMPLES.exists():
        raise SystemExit("沒有 samples/sessions（不進 git；用 scripts/redact-samples.py 產生）")
    return sorted(d for d in SAMPLES.iterdir() if (d / "hooks.jsonl").exists())


def pct(values, p):
    v = sorted(values)
    return v[min(len(v) - 1, int(round(p / 100 * (len(v) - 1))))]
