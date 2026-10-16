use std::io::{self, BufRead};

/// 一個 HTTP 請求的必要部分。
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

/// payload 上限；超過就當壞請求（真實 hook 不會這麼大）。
const MAX_BODY: usize = 64 * 1024 * 1024;

/// 讀一個 HTTP/1.1 請求（支援 Content-Length 與 chunked）。
pub fn read_request(r: &mut impl BufRead) -> io::Result<Request> {
    let line = read_line(r)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = None;
    let mut chunked = false;
    loop {
        let h = read_line(r)?;
        if h.is_empty() {
            break;
        }
        if let Some((name, value)) = h.split_once(':') {
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => {
                    content_length = Some(value.parse::<usize>().map_err(bad)?);
                }
                "transfer-encoding" => {
                    chunked = value.to_ascii_lowercase().contains("chunked");
                }
                _ => {}
            }
        }
    }

    let body = if chunked {
        read_chunked(r)?
    } else {
        let n = content_length.unwrap_or(0);
        if n > MAX_BODY {
            return Err(bad("body too large"));
        }
        let mut body = vec![0; n];
        r.read_exact(&mut body)?;
        body
    };
    Ok(Request { method, path, body })
}

fn read_chunked(r: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let size_line = read_line(r)?;
        let size_hex = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(bad)?;
        if size == 0 {
            // 略過 trailer 直到空行
            while !read_line(r)?.is_empty() {}
            return Ok(body);
        }
        if body.len() + size > MAX_BODY {
            return Err(bad("body too large"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        r.read_exact(&mut body[start..])?;
        read_line(r)?; // chunk 後面的 CRLF
    }
}

fn read_line(r: &mut impl BufRead) -> io::Result<String> {
    let mut buf = Vec::new();
    if r.read_until(b'\n', &mut buf)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed",
        ));
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    String::from_utf8(buf).map_err(bad)
}

fn bad(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// 永遠回 200 空 body：不帶任何 JSON，Claude Code 才不會把它當成決策或上下文。
pub const OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
