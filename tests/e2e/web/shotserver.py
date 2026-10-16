"""前端 headless 截圖與互動自我測試用的小 server（只綁 127.0.0.1）。

用法：python3 shotserver.py <web 目錄> <port> <結果檔>

- 其他路徑照常 serve <web 目錄> 的靜態檔。
- /shot.html?<query>：index.html 加上兩樣東西再送出：
  1. requestAnimationFrame 的 setTimeout 版：headless Firefox 的 --screenshot 在 load 之前不跑 rAF，
     不換掉的話面板永遠不會畫。
  2. 一張要等 delay 秒才回的 1×1 圖片（`&delay=<秒>`），把 load 事件往後拖，截圖才會等到資料進來。
  query 含 `selftest` 時另外注入 selftest.js。
- POST /__result：selftest.js 跑完把結果文字送回來，寫進 <結果檔>。
"""

import functools
import http.server
import pathlib
import sys
import time

WEB, PORT, RESULT = sys.argv[1], int(sys.argv[2]), pathlib.Path(sys.argv[3])
HERE = pathlib.Path(__file__).parent

RAF = (
    "<script>window.requestAnimationFrame=(cb)=>setTimeout(()=>cb(performance.now()),16);"
    "window.cancelAnimationFrame=(t)=>clearTimeout(t);</script>"
)


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send(self, body, ctype, code=200):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/__slow"):
            time.sleep(float(self.path.split("=")[-1]))
            return self.send(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;", "image/gif")
        if self.path.startswith("/__selftest.js"):
            return self.send((HERE / "selftest.js").read_bytes(), "text/javascript; charset=utf-8")
        if self.path.startswith("/shot.html"):
            delay = "3"
            if "delay=" in self.path:
                delay = self.path.split("delay=")[1].split("&")[0]
            head = RAF + ('<script src="/__selftest.js"></script>' if "selftest" in self.path else "")
            html = (pathlib.Path(WEB) / "index.html").read_text(encoding="utf-8")
            html = html.replace("<head>", "<head>" + head).replace(
                "</body>", f'<img src="/__slow?d={delay}" width=1 height=1></body>'
            )
            return self.send(html.encode(), "text/html; charset=utf-8")
        return super().do_GET()

    def do_POST(self):
        if self.path != "/__result":
            return self.send(b"", "text/plain", 404)
        n = int(self.headers.get("Content-Length", "0"))
        RESULT.write_bytes(self.rfile.read(n))
        return self.send(b"ok", "text/plain")


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), functools.partial(Handler, directory=WEB)).serve_forever()
