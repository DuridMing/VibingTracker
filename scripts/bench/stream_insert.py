#!/usr/bin/env python3
"""量測 3：前端串流已有 3000 行時，每幀插入 20 行的耗時（#637）。

起一個只綁 127.0.0.1 的小伺服器提供 stream_insert.html，用 headless Firefox（暫存 profile 放 target/tmp）
打開；頁面量完把結果 POST 回來。比較三種做法：直接操作 DOM、加上 content-visibility、虛擬列表。

用法：scripts/bench/stream_insert.py [瀏覽器執行檔，預設 firefox]
"""

import http.server
import json
import shutil
import subprocess
import sys
import threading
from pathlib import Path

from common import ROOT, free_port, pct

HERE = Path(__file__).resolve().parent
result = {}
done = threading.Event()


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = (HERE / "stream_insert.html").read_bytes()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        result.update(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
        self.send_response(204)
        self.end_headers()
        done.set()

    def log_message(self, *a):
        pass


def main():
    browser = sys.argv[1] if len(sys.argv) > 1 else "firefox"
    port = free_port()
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    profile = ROOT / "target" / "tmp" / "bench-firefox-profile"
    shutil.rmtree(profile, ignore_errors=True)
    profile.mkdir(parents=True)
    # Firefox 預設把 performance.now() 降到 1 ms 精度（防指紋），量測時關掉
    (profile / "user.js").write_text('user_pref("privacy.reduceTimerPrecision", false);\n')
    proc = subprocess.Popen(
        [browser, "--headless", "--no-remote", "--profile", str(profile), "--width", "1280", "--height", "900",
         f"http://127.0.0.1:{port}/"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        if not done.wait(180):
            raise SystemExit("180 秒內沒有收到結果")
    finally:
        proc.terminate()
        proc.wait(timeout=10)
        srv.shutdown()
    print(result["ua"])
    print("每幀：append 20 行＋刪最舊 20 行，維持 3000 行，貼底捲動；150 幀")
    for r in result["results"]:
        s, f = r["scripts"], r["frames"]
        print(
            f"{r['name']:<19} 插入＋版面 p50 {pct(s, 50):6.2f} ms  p95 {pct(s, 95):6.2f} ms  最高 {max(s):6.2f} ms  │  "
            f"幀間隔 p50 {pct(f, 50):5.1f} ms  p95 {pct(f, 95):5.1f} ms  掉幀（> 25 ms）{sum(x > 25 for x in f)}/{len(f)}"
        )


if __name__ == "__main__":
    main()
