#!/usr/bin/env python3
"""重算階段 0 的量測結果（#624、#625、#626），確認 PLAN 依據的事實在目前的 Claude Code 版本仍成立。

用法：python3 -I tests/replay/analyze-samples.py [samples 目錄，預設 samples]

讀 scripts/redact-samples.py 產生的 samples/sessions/*/，每一節對應 PLAN 的一個結論；
最後一節列出「檢查項目」，任何一項不成立就以非 0 結束，表示 PLAN 要重新檢討。
"""

import json
import sys
from collections import Counter
from pathlib import Path

# 解除「等你」的事件：同一工具（tool_name＋tool_input）的結果，或任何表示這一批工具／這一回合已結束的事件
WAIT_CLEAR_SAME_TOOL = ("PostToolUse", "PostToolUseFailure", "PermissionDenied")
WAIT_CLEAR_ANY = ("PostToolBatch", "UserPromptSubmit", "Stop", "SessionEnd")


def load(path: Path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.open(encoding="utf-8") if line.strip()]


def stats(xs):
    xs = sorted(xs)
    if not xs:
        return "n=0"
    return f"n={len(xs)} min={xs[0]} median={xs[len(xs) // 2]} max={xs[-1]}"


def main():
    root = Path(sys.argv[1] if len(sys.argv) > 1 else "samples")
    sessions = sorted(p for p in (root / "sessions").iterdir() if p.is_dir())
    checks = {}

    def check(name, ok):
        checks[name] = checks.get(name, True) and bool(ok)

    via = Counter()
    tool_id = Counter()
    pairs = {}
    pre_lead, think_lag, text_lag, result_lag = [], [], [], []
    line_blocks = Counter()
    ts_inversions = 0
    idle_delays = []
    probes = Counter()
    wait_lag = []
    wait_clear = []

    for s in sessions:
        hooks = load(s / "hooks.jsonl")
        arrivals = load(s / "transcript-arrivals.jsonl")
        transcript = load(s / "transcript.jsonl")

        # ── hook 傳輸（#624）
        for r in hooks:
            via[(r["event"], r["via"])] += 1
            p = r.get("payload") or {}
            if r["via"] == "http" and r["event"] in ("PreToolUse", "PostToolUse", "PostToolUseFailure", "PermissionRequest", "PermissionDenied"):
                tool_id[(r["event"], "tool_use_id" in p)] += 1
            key = (s.name, r["event"], json.dumps(p, sort_keys=True))
            pairs.setdefault(key, {})[r["via"]] = r["ts_seen_ms"]
            for k in ("probe_sync", "probe_before_reply", "probe_after_reply"):
                v = r.get(k)
                if v:
                    ok = v.get("old_count", 0) >= 1 if r["event"] == "PreToolUse" else v.get("new_count", 0) >= 1
                    probes[(k, ok)] += 1

        http = [r for r in hooks if r["via"] == "http"]
        pre = {r["payload"]["tool_use_id"]: r["ts_seen_ms"] for r in http if r["event"] == "PreToolUse"}
        post = {r["payload"]["tool_use_id"]: r["ts_seen_ms"] for r in http if r["event"] in ("PostToolUse", "PostToolUseFailure")}

        # ── 等你：PermissionRequest 相對 PreToolUse（同 tool_name＋tool_input）
        pre_rows = [r for r in http if r["event"] == "PreToolUse"]
        for r in http:
            if r["event"] != "PermissionRequest":
                continue
            p = r["payload"]
            match = [x for x in pre_rows if x["payload"].get("tool_name") == p.get("tool_name")
                     and x["payload"].get("tool_input") == p.get("tool_input") and x["ts_seen_ms"] <= r["ts_seen_ms"]]
            if match:
                wait_lag.append(r["ts_seen_ms"] - match[-1]["ts_seen_ms"])

        # ── 等你的解除：PermissionRequest 之後第一個能解除它的事件（P3 #645）
        for i, r in enumerate(http):
            if r["event"] != "PermissionRequest":
                continue
            p = r["payload"]
            hit = None
            for x in http[i + 1:]:
                xp = x["payload"]
                same = xp.get("tool_name") == p.get("tool_name") and xp.get("tool_input") == p.get("tool_input")
                if x["event"] in WAIT_CLEAR_SAME_TOOL and same:
                    hit = x["event"]
                elif x["event"] in WAIT_CLEAR_ANY:
                    hit = x["event"]
                if hit:
                    wait_clear.append((p.get("permission_mode"), p.get("tool_name"), hit, x["ts_seen_ms"] - r["ts_seen_ms"]))
                    break
            if not hit:
                wait_clear.append((p.get("permission_mode"), p.get("tool_name"), None, None))

        # ── idle_prompt 距離前一個非 Notification 事件
        last = None
        for r in sorted(hooks, key=lambda r: r["ts_seen_ms"]):
            if r["event"] == "Notification" and r["via"] == "http" and r["payload"].get("notification_type") == "idle_prompt":
                if last is not None:
                    idle_delays.append(round((r["ts_seen_ms"] - last) / 1000))
            elif r["event"] != "Notification":
                last = r["ts_seen_ms"]

        # ── transcript 到達順序（#625）
        live = [a for a in arrivals if "line_no" in a and not a.get("backfill")]
        tool_use_ts, result_ts = {}, {}
        pending = []
        for a in live:
            blocks = a.get("blocks") or []
            if a.get("type") == "assistant":
                line_blocks[tuple(blocks)] += 1
            if a.get("type") == "assistant" and blocks[:1] in (["thinking"], ["text"]):
                pending.append(a)
            elif a.get("type") == "assistant" and "tool_use" in blocks:
                for i in a.get("tool_ids") or []:
                    tool_use_ts.setdefault(i, a["ts_seen_ms"])
                    if i in pre:
                        for p in pending:
                            (think_lag if p["blocks"][0] == "thinking" else text_lag).append(p["ts_seen_ms"] - pre[i])
                pending = []
            elif a.get("type") == "user":
                pending = []
                if "tool_result" in blocks:
                    for i in a.get("tool_ids") or []:
                        result_ts.setdefault(i, a["ts_seen_ms"])
        pre_lead += [tool_use_ts[i] - pre[i] for i in pre if i in tool_use_ts]
        result_lag += [result_ts[i] - post[i] for i in post if i in result_ts]

        stamps = [t.get("timestamp") for t in transcript if t.get("timestamp")]
        ts_inversions += sum(1 for a, b in zip(stamps, stamps[1:]) if b < a)

    print(f"== samples：{len(sessions)} 個 session（{root}）\n")

    print("== hook 傳輸（#624）")
    for ev in sorted({e for e, _ in via}):
        print(f"  {ev:20} http={via[(ev, 'http')]:4} command={via[(ev, 'command')]:4}")
    both = [v for v in pairs.values() if "http" in v and "command" in v]
    only_cmd = Counter(k[1] for k, v in pairs.items() if set(v) == {"command"})
    diffs = [v["command"] - v["http"] for v in both]
    print(f"  http 與 command payload 逐字相同的組數：{len(both)}；只有 command 的事件：{dict(only_cmd)}")
    print(f"  command − http 送達時間差 ms：{stats(diffs)}")
    for (ev, has), n in sorted(tool_id.items()):
        print(f"  {ev:20} tool_use_id {'有' if has else '無'}：{n}")
    check("SessionStart 不送 http hook", via[("SessionStart", "http")] == 0 and via[("SessionStart", "command")] > 0)
    check("Pre/PostToolUse 都有 tool_use_id", all(has for (ev, has) in tool_id if ev in ("PreToolUse", "PostToolUse", "PostToolUseFailure")))

    print("\n== transcript 順序（#625）")
    print(f"  assistant 行的 block 組合：{dict(line_blocks.most_common())}")
    print(f"  transcript tool_use − PreToolUse ms（正＝hook 先到）：{stats(pre_lead)}")
    print(f"  thinking − PreToolUse ms：{stats(think_lag)}")
    print(f"  text − PreToolUse ms：{stats(text_lag)}")
    print(f"  transcript tool_result − PostToolUse ms：{stats(result_lag)}")
    print(f"  timestamp 倒退次數：{ts_inversions}")
    check("每行只有一個 block", all(len(b) <= 1 for b in line_blocks))
    check("PreToolUse 都比 transcript tool_use 先到（方案 C 的前提）", pre_lead and all(x > 0 for x in pre_lead))
    check("transcript 晚到不超過 10 秒（降級門檻）", all(x < 10000 for x in pre_lead + think_lag + text_lag))
    slow = [x for x in pre_lead + think_lag + text_lag if x >= 5000]
    if slow:
        print(f"  注意：{len(slow)} 筆晚到超過 5 秒：{sorted(slow)}")

    print("\n== 等你與閒置（#626 未決 4）")
    print(f"  PermissionRequest − PreToolUse ms：{stats(wait_lag)}")
    print(f"  idle_prompt 距離前一個事件 s：{stats(idle_delays)}")
    check("PermissionRequest 在 1 秒內到", wait_lag and all(x < 1000 for x in wait_lag))

    print("\n== 等你的解除（P3 #645；依 permission_mode 分組）")
    groups = {}
    for mode, tool, ev, ms in wait_clear:
        groups.setdefault((mode or "-", tool, ev or "（找不到）"), []).append(ms)
    for (mode, tool, ev), xs in sorted(groups.items()):
        print(f"  {mode:12} {tool:16} → {ev:20} ms：{stats([x for x in xs if x is not None])}")
    check("每個 PermissionRequest 都找得到解除事件", all(ev for _, _, ev, _ in wait_clear))

    print("\n== Edit 讀檔（#626 未決 3）")
    for (k, ok), n in sorted(probes.items()):
        print(f"  {k:20} {'符合' if ok else '異常'}：{n}")
    check("回應前讀檔都讀到修改前內容", probes and not probes[("probe_before_reply", False)] and not probes[("probe_sync", False)])

    print("\n== 檢查項目")
    for name, ok in checks.items():
        print(f"  {'✓' if ok else '✗'} {name}")
    sys.exit(0 if all(checks.values()) else 1)


if __name__ == "__main__":
    main()
