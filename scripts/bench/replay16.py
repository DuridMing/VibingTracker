#!/usr/bin/env python3
"""量測 2：16 個 session 同時跑時，collector 的 CPU 與 tokio 排程延遲（#637）。

把 samples/ 的真實 session 輪流分給 16 個虛擬 session（換 session id 與 transcript 路徑），
照原本的時間間隔同時回放：hook 用 http 送進來，transcript 的行照實測的到達時間 append 到檔案，
讓 collector 用真的 tail 讀。SPEED > 1 時把時間軸壓縮（例如 10 = 十倍速，當壓力測試）。
每 0.5 秒取樣 /proc 的 CPU 與 /healthz 的 schedLagMs。

用法：scripts/bench/replay16.py [SESSIONS=16] [SPEED=1]
"""

import json
import sys
import threading
import time
from pathlib import Path

from common import Collector, pct, read_jsonl, sample_dirs


def build_timeline(sample: Path, sid: str, proj: Path):
    """回傳 [(相對秒, 動作)]；動作是 ("hook", bytes) 或 ("line", 檔案, 文字)。"""
    hooks = read_jsonl(sample / "hooks.jsonl")
    if not hooks:
        return []
    old_sid = hooks[0]["payload"]["session_id"]
    main = proj / f"{sid}.jsonl"
    sub_dir = proj / sid / "subagents"

    def rewrite(p):
        p = dict(p)
        p["session_id"] = sid
        if "transcript_path" in p:
            p["transcript_path"] = str(main)
        if p.get("agent_transcript_path"):
            p["agent_transcript_path"] = str(sub_dir / Path(p["agent_transcript_path"]).name)
        return p

    events = []
    for h in hooks:
        # SessionStart 實際上由 ensure 轉送（command hook）；其他事件只取 http 那一份，避免重複
        if h.get("via") == "http" or h["payload"].get("hook_event_name") == "SessionStart":
            events.append((h["ts_seen_ms"], ("hook", json.dumps(rewrite(h["payload"])).encode())))

    files = [(main, sample / "transcript.jsonl", sample / "transcript-arrivals.jsonl")]
    for f in sorted((sample / "subagents").glob("agent-*.jsonl")) if (sample / "subagents").exists() else []:
        aid = f.stem.removeprefix("agent-")
        files.append((sub_dir / f.name, f, sample / f"subagent-{aid}-arrivals.jsonl"))
    for target, src, arrivals in files:
        lines = src.read_text().splitlines() if src.exists() else []
        first = {}
        for a in read_jsonl(arrivals):
            first.setdefault(a["line_no"], a["ts_seen_ms"])
        for no, text in enumerate(lines, 1):
            if no in first:
                events.append((first[no], ("line", target, text.replace(old_sid, sid) + "\n")))

    events.sort(key=lambda e: e[0])
    t0 = events[0][0] if events else 0
    return [((t - t0) / 1000, a) for t, a in events]


def run_session(c, timeline, speed, start, errors):
    for at, action in timeline:
        delay = start + at / speed - time.perf_counter()
        if delay > 0:
            time.sleep(delay)
        try:
            if action[0] == "hook":
                status, _ = c.post_hook(action[1])
                if status != 200:
                    errors.append(status)
            else:
                _, target, text = action
                target.parent.mkdir(parents=True, exist_ok=True)
                with open(target, "a") as f:
                    f.write(text)
        except Exception as e:  # noqa: BLE001 — 量測工具：記下來繼續
            errors.append(repr(e))


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 16
    speed = float(sys.argv[2]) if len(sys.argv) > 2 else 1.0
    # 沒有 transcript 到達時間的樣本無法照實回放（transcript 不會出現，卡片會被降級），和回放測試一樣略過
    samples = [d for d in sample_dirs() if (d / "transcript-arrivals.jsonl").exists()]
    with Collector(f"replay{n}-x{speed:g}") as c:
        proj = c.home / "projects"
        timelines = [build_timeline(samples[i % len(samples)], f"bench-{i:02d}", proj) for i in range(n)]
        timelines = [t for t in timelines if t]
        span = max(t[-1][0] for t in timelines) / speed
        total = sum(len(t) for t in timelines)
        print(f"{len(timelines)} 個 session、{total} 個輸入（hook＋transcript 行），速度 ×{speed:g}，約 {span:.0f} 秒")

        errors, threads = [], []
        start = time.perf_counter() + 0.5
        for t in timelines:
            th = threading.Thread(target=run_session, args=(c, t, speed, start, errors), daemon=True)
            th.start()
            threads.append(th)

        cpu_samples, lag, tails = [], [], 0
        prev_cpu, prev_t = c.cpu_seconds(), time.perf_counter()
        while any(th.is_alive() for th in threads):
            time.sleep(0.5)
            now_cpu, now_t = c.cpu_seconds(), time.perf_counter()
            cpu_samples.append(100 * (now_cpu - prev_cpu) / (now_t - prev_t))
            prev_cpu, prev_t = now_cpu, now_t
            h = c.healthz()
            lag.append(h.get("schedLagMs", 0))
            tails = max(tails, h.get("tails", 0))
        time.sleep(1.5)  # 讓最後的 tail 與 flush 跑完
        h = c.healthz()

        all_events = [e for p in (c.home / "events").glob("*.jsonl") for e in read_jsonl(p)]
        events = len(all_events)
        placed = [e["payload"]["placement"] for e in all_events if e["type"] == "tool.placed"]
        print(f"CPU（單核 %）：平均 {sum(cpu_samples) / len(cpu_samples):.1f}、p95 {pct(cpu_samples, 95):.1f}、最高 {max(cpu_samples):.1f}")
        print(f"tokio 排程延遲（每 0.5 秒取樣）：p95 {pct(lag, 95)} ms、最高 {max(lag)} ms")
        print(f"同時 tail 的檔案：最多 {tails}；RSS {c.rss_mb():.1f} MB；寫入 {events} 個 Event；sessions {h.get('sessions')}")
        print(f"卡片進串流：stream {placed.count('stream')}、fallback {placed.count('fallback')}（實測資料應該是 0）")
        if errors:
            print(f"錯誤 {len(errors)} 個：{errors[:5]}")


if __name__ == "__main__":
    main()
