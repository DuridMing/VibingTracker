#!/usr/bin/env python3
"""把 hook-capture 的原始輸出遮蔽後放進 samples/（samples/ 不進 git）。

用法：python3 -I scripts/redact-samples.py <capture 目錄>... [--out samples]

每個 capture 目錄底下的 <session_id>/ 會變成 samples/sessions/<id 前 8 碼>/，
另外從所有 session 挑出每種 hook 事件的第一筆（command 版）放 samples/hooks/<事件>.json。

遮蔽規則：
- 使用者名稱、家目錄、主機名稱 → user、/home/user、host
- email（noreply@anthropic.com 除外）→ user@example.com
- 常見 token 格式 → [REDACTED-TOKEN]
- transcript 的 attachment 行只留結構欄位與 attachment.type，內容換成 [redacted]
  （裡面有 MCP 說明、session context、環境資訊等與測試無關的私人內容）
"""

import argparse
import getpass
import json
import os
import re
import socket
import sys
from pathlib import Path

KEEP_ATTACHMENT_TYPES = {"date", "model", "total_tokens_reminder"}
ATTACHMENT_KEEP_KEYS = {"type", "uuid", "parentUuid", "timestamp", "sessionId", "isSidechain", "agentId"}

TOKEN_PATTERNS = [
    r"sk-ant-[A-Za-z0-9_-]{10,}",
    r"sk-[A-Za-z0-9]{20,}",
    r"gh[pousr]_[A-Za-z0-9]{20,}",
    r"github_pat_[A-Za-z0-9_]{20,}",
    r"xox[abprs]-[A-Za-z0-9-]{10,}",
    r"AKIA[0-9A-Z]{16}",
    r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{16,}",
]
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
KEEP_EMAILS = {"noreply@anthropic.com"}


def build_replacer():
    user = getpass.getuser()
    home = os.path.expanduser("~")
    host = socket.gethostname()
    pairs = [(home, "/home/user"), (f"-home-{user}-", "-home-user-")]
    if host:
        pairs.append((host, "host"))
    tokens = [re.compile(p) for p in TOKEN_PATTERNS]
    user_re = re.compile(re.escape(user), re.IGNORECASE) if len(user) >= 3 else None

    def scrub(s: str) -> str:
        for old, new in pairs:
            s = s.replace(old, new)
        for t in tokens:
            s = t.sub("[REDACTED-TOKEN]", s)
        s = EMAIL.sub(lambda m: m.group(0) if m.group(0) in KEEP_EMAILS else "user@example.com", s)
        if user_re:
            s = user_re.sub("user", s)
        return s

    return scrub


def walk(v, scrub):
    if isinstance(v, str):
        return scrub(v)
    if isinstance(v, list):
        return [walk(x, scrub) for x in v]
    if isinstance(v, dict):
        return {scrub(k): walk(x, scrub) for k, x in v.items()}
    return v


def redact_transcript_line(row):
    if row.get("type") != "attachment":
        return row
    att = row.get("attachment") or {}
    kind = att.get("type")
    if kind in KEEP_ATTACHMENT_TYPES:
        return row
    out = {k: v for k, v in row.items() if k in ATTACHMENT_KEEP_KEYS}
    out["type"] = "attachment"
    out["attachment"] = {"type": kind, "redacted": True}
    return out


def redact_jsonl(src: Path, dst: Path, scrub, transcript: bool):
    dst.parent.mkdir(parents=True, exist_ok=True)
    with src.open(encoding="utf-8") as f, dst.open("w", encoding="utf-8") as g:
        for line in f:
            line = line.rstrip("\n")
            if not line:
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                g.write(scrub(line) + "\n")
                continue
            if transcript:
                row = redact_transcript_line(row)
            g.write(json.dumps(walk(row, scrub), ensure_ascii=False) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("captures", nargs="+", type=Path)
    ap.add_argument("--out", type=Path, default=Path("samples"))
    args = ap.parse_args()
    scrub = build_replacer()
    first_of_event = {}

    for cap in args.captures:
        for sdir in sorted(p for p in cap.iterdir() if p.is_dir()):
            dst = args.out / "sessions" / sdir.name[:8]
            for src in sorted(sdir.rglob("*.jsonl")):
                rel = src.relative_to(sdir)
                if rel.parts[0] == "transcript-dir":
                    rel = Path(*rel.parts[1:])
                is_transcript = src.name == "transcript.jsonl" or "subagents" in src.parts
                redact_jsonl(src, dst / rel, scrub, is_transcript)
            hooks = sdir / "hooks.jsonl"
            if hooks.exists():
                for line in hooks.open(encoding="utf-8"):
                    row = json.loads(line)
                    if row.get("via") == "command" and "payload" in row:
                        first_of_event.setdefault(row["event"], row["payload"])
            print(f"{sdir.name[:8]} → {dst}")

    hdir = args.out / "hooks"
    hdir.mkdir(parents=True, exist_ok=True)
    for event, payload in sorted(first_of_event.items()):
        (hdir / f"{event}.json").write_text(
            json.dumps(walk(payload, scrub), ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
    print(f"hooks：{len(first_of_event)} 種事件 → {hdir}")

    # 最後檢查：遮蔽後不能再出現使用者名稱與主機名稱
    leaks = [getpass.getuser(), socket.gethostname()]
    bad = [
        str(p) for p in args.out.rglob("*") if p.is_file()
        and any(x and x.lower() in p.read_text(encoding="utf-8", errors="replace").lower() for x in leaks)
    ]
    if bad:
        print("仍有未遮蔽的內容：", *bad, sep="\n  ", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
