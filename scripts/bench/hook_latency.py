#!/usr/bin/env python3
"""量測 1：hook 從送出到收到回應的時間（#637）。

把 samples/ 裡全部真實的 http hook payload，對隔離的 collector 送 ROUNDS 輪：
- 循序：一次一個，像單一 session。
- 16 路並行：像 16 個 session 同時送。
每個請求都開一條新連線（Claude Code 的 http hook 就是這樣），這是最差的情況。
Claude Code 那一側實際看到的時間另外用 `claude --debug-file` 量（見 PLAN 的結果）。

用法：scripts/bench/hook_latency.py [ROUNDS]
"""

import json
import os
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

from common import BIN, ROOT, Collector, pct, read_jsonl, sample_dirs

WRAPPER = ROOT / "plugin" / "bin" / "coding-lab"


def payloads():
    out = []
    for d in sample_dirs():
        for h in read_jsonl(d / "hooks.jsonl"):
            if h.get("via") == "http":
                # 換 session id，避免和其他輪混在一起時 tail 到真的 transcript
                p = dict(h["payload"])
                p["session_id"] = "bench-" + p["session_id"][:8]
                p.pop("transcript_path", None)
                out.append(json.dumps(p).encode())
    return out


def report(name, times):
    ms = [t * 1000 for t in times]
    print(
        f"{name:<10} n={len(ms):>5}  p50={pct(ms, 50):6.2f} ms  p95={pct(ms, 95):6.2f} ms  "
        f"p99={pct(ms, 99):6.2f} ms  max={max(ms):7.2f} ms"
    )


def main():
    rounds = int(sys.argv[1]) if len(sys.argv) > 1 else 5
    bodies = payloads()
    print(f"payload：{len(bodies)} 個（{len(sample_dirs())} 個樣本 session）× {rounds} 輪")
    with Collector("hook-latency") as c:
        for b in bodies[:50]:  # 暖機
            c.post_hook(b)
        seq = []
        for _ in range(rounds):
            for b in bodies:
                status, t = c.post_hook(b)
                assert status == 200, status
                seq.append(t)
        report("循序", seq)

        with ThreadPoolExecutor(16) as pool:
            par = [t for _, t in pool.map(c.post_hook, bodies * rounds)]
        report("16 路並行", par)
        print(f"collector：CPU {c.cpu_seconds():.2f} s、RSS {c.rss_mb():.1f} MB、healthz {c.healthz()}")

        # SessionStart：plugin wrapper → ensure → 轉送（collector 已在跑）
        warm = [ensure(c.home, i) for i in range(20)]
        report("ensure 熱", warm)

    # collector 沒在跑：ensure 要先拉起它（每次用新的資料目錄）
    cold = []
    for i in range(3):
        with Collector(f"ensure-cold-{i}") as fresh:
            pass  # 只借它產生 config.toml 與空的資料目錄
        cold.append(ensure(fresh.home, 100 + i))
        stop_spawned(fresh.home)
    report("ensure 冷", cold)


def ensure(home, i):
    start = json.dumps({"session_id": f"bench-ensure-{i}", "hook_event_name": "SessionStart",
                        "source": "startup", "cwd": "/bench"}).encode()
    env = dict(os.environ, CODING_LAB_HOME=str(home), CODING_LAB_BIN=str(BIN))
    t = time.perf_counter()
    r = subprocess.run([str(WRAPPER), "ensure"], input=start, env=env, capture_output=True)
    dt = time.perf_counter() - t
    assert r.returncode == 0 and not r.stdout, r
    return dt


def stop_spawned(home):
    info = home / "collector.json"
    deadline = time.time() + 5
    while not info.exists() and time.time() < deadline:
        time.sleep(0.02)
    if info.exists():
        os.kill(json.loads(info.read_text())["pid"], 15)


if __name__ == "__main__":
    main()
