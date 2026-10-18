//! web 驗證（#667，contracts.md §6–§7）：cookie 簽章與竄改、一次性登入連結、速率限制、
//! `auth` 兩種模式 × Bearer／cookie／Origin、WS 握手與 scope。時間注入（假時鐘），不 sleep。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use coding_lab::auth::{self, CookieError, RateLimiter};
use coding_lab::config::AuthMode;
use coding_lab::keys::{self, KeyStore, Scope};
use coding_lab::ws::{WebState, serve_web};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

static N: AtomicU32 = AtomicU32::new(0);
const NOW: u64 = 1_760_000_000_000;
const MIN: u64 = 60_000;

fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "auth-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ── HMAC 與 cookie ────────────────────────────────────────────────────────

#[test]
fn hmac_sha256_matches_the_known_vector() {
    let mac = auth::hmac_sha256(b"key", b"The quick brown fox jumps over the lazy dog");
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
    );
}

#[test]
fn cookie_roundtrip_tamper_expiry_and_wrong_secret() {
    let secret = [3u8; 32];
    let value = auth::sign_cookie(&secret, "abcdefgh", NOW + 1000);
    assert_eq!(
        auth::parse_cookie(&secret, &value, NOW),
        Ok(("abcdefgh".to_string(), NOW + 1000))
    );
    assert_eq!(
        auth::parse_cookie(&secret, &value, NOW + 1000),
        Err(CookieError::Expired),
        "到期那一刻就不算"
    );
    // 改 key id
    let tampered = value.replacen("abcdefgh", "abcdefgX", 1);
    assert_eq!(
        auth::parse_cookie(&secret, &tampered, NOW),
        Err(CookieError::BadSignature)
    );
    // 改簽章最後一個字
    let mut t = value.clone();
    let last = t.pop().unwrap();
    t.push(if last == '0' { '1' } else { '0' });
    assert_eq!(
        auth::parse_cookie(&secret, &t, NOW),
        Err(CookieError::BadSignature)
    );
    // 改到期時間
    let parts: Vec<&str> = value.split('.').collect();
    let later = format!("{}.{}.{}", parts[0], NOW + 999_999, parts[2]);
    assert_eq!(
        auth::parse_cookie(&secret, &later, NOW),
        Err(CookieError::BadSignature)
    );
    // 別把 server secret
    assert_eq!(
        auth::parse_cookie(&[4u8; 32], &value, NOW),
        Err(CookieError::BadSignature)
    );
    assert_eq!(
        auth::parse_cookie(&secret, "garbage", NOW),
        Err(CookieError::Malformed)
    );
    assert_eq!(
        auth::parse_cookie(&secret, "a.b", NOW),
        Err(CookieError::Malformed)
    );
}

// ── 一次性登入連結 ────────────────────────────────────────────────────────

#[test]
fn once_link_is_single_use_and_expires() {
    let home = tmp("once");
    let mut rng = keys::test_rng(5);
    let code = auth::create_once_link(&home, "abcdefgh", NOW, &mut rng).unwrap();
    assert!(code.len() >= 32 && code.chars().all(|c| c.is_ascii_alphanumeric()));
    let text = std::fs::read_to_string(home.join(auth::LINKS_FILE)).unwrap();
    assert!(!text.contains(&code), "檔裡只有雜湊");

    assert_eq!(
        auth::consume_once_link(&home, &code, NOW + 1000).unwrap(),
        Some("abcdefgh".to_string())
    );
    assert_eq!(
        auth::consume_once_link(&home, &code, NOW + 2000).unwrap(),
        None,
        "第二次就沒有了"
    );

    let code2 = auth::create_once_link(&home, "abcdefgh", NOW, &mut rng).unwrap();
    assert_eq!(
        auth::consume_once_link(&home, &code2, NOW + auth::ONCE_TTL_MS).unwrap(),
        None,
        "5 分鐘到就失效"
    );
    assert_eq!(auth::consume_once_link(&home, "nope", NOW).unwrap(), None);
}

#[cfg(unix)]
#[test]
fn once_link_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = tmp("once-mode");
    auth::create_once_link(&home, "abcdefgh", NOW, &mut keys::test_rng(1)).unwrap();
    let mode = std::fs::metadata(home.join(auth::LINKS_FILE))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

// ── 速率限制 ──────────────────────────────────────────────────────────────

#[test]
fn rate_limiter_allows_ten_failures_per_minute_per_ip() {
    let mut rl = RateLimiter::new(auth::LOGIN_FAILS_PER_MINUTE, MIN);
    let a: std::net::IpAddr = "100.64.0.7".parse().unwrap();
    let b: std::net::IpAddr = "100.64.0.8".parse().unwrap();
    for i in 0..10 {
        assert!(rl.allowed(a, NOW + i), "第 {i} 次失敗前還能試");
        rl.record_failure(a, NOW + i);
    }
    assert!(!rl.allowed(a, NOW + 10), "10 次失敗後擋");
    assert!(rl.allowed(b, NOW + 10), "別的 IP 不受影響");
    assert!(rl.allowed(a, NOW + MIN + 1), "一分鐘後解除");
}

// ── 整合：auth = always 在 loopback 上（和遠端走同一條路） ───────────────────

struct Server {
    addr: SocketAddr,
    home: PathBuf,
    store: KeyStore,
    view: String,
    operate: String,
}

async fn start(mode: AuthMode) -> Server {
    let home = tmp("srv");
    let mut store = KeyStore::new_with_secret([9u8; 32]);
    let mut rng = keys::test_rng(42);
    let view = store.create("view", Scope::View, None, NOW, &mut rng);
    let operate = store.create("op", Scope::Operate, None, NOW, &mut rng);
    store.save(&home).unwrap();
    let state = WebState::new_with_auth(None, None, 8, home.clone(), mode);
    let (addr, _) = serve_web("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap();
    Server {
        addr,
        home,
        store,
        view,
        operate,
    }
}

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let code: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap();
    (code, head.to_string(), body.to_string())
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn cookie_of(head: &str) -> String {
    let sc = header(head, "set-cookie").expect("要有 Set-Cookie");
    sc.split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn always_mode_requires_a_key_except_healthz_and_login() {
    let s = start(AuthMode::Always).await;
    let (code, _, _) = http(s.addr, "GET", "/api/sessions", &[], "").await;
    assert_eq!(code, 401, "API 沒金鑰一律 401");
    let (code, _, _) = http(s.addr, "GET", "/healthz", &[], "").await;
    assert_eq!(code, 200, "healthz 不用金鑰");
    let (code, head, _) = http(s.addr, "GET", "/", &[], "").await;
    assert_eq!(code, 303, "網頁導去登入");
    assert_eq!(header(&head, "location"), Some("/auth/login"));
    let (code, head, body) = http(s.addr, "GET", "/auth/login", &[], "").await;
    assert_eq!(code, 200);
    assert!(
        header(&head, "content-type")
            .unwrap()
            .starts_with("text/html")
    );
    assert!(
        body.contains("<form") && body.contains("clk_"),
        "登入頁有表單與提示：{body}"
    );
}

#[tokio::test]
async fn bearer_works_and_revocation_is_immediate() {
    let mut s = start(AuthMode::Always).await;
    let bearer = format!("Bearer {}", s.view);
    let (code, _, _) = http(
        s.addr,
        "GET",
        "/api/sessions",
        &[("Authorization", &bearer)],
        "",
    )
    .await;
    assert_eq!(code, 200);
    let (code, _, _) = http(
        s.addr,
        "GET",
        "/api/sessions",
        &[("Authorization", "Bearer clk_zzzzzzzz_x")],
        "",
    )
    .await;
    assert_eq!(code, 401);

    // 撤銷：另一個程序（CLI）改檔，collector 下一次請求就看到
    let id = keys::parse_token(&s.view).unwrap().0.to_string();
    assert!(s.store.revoke(&id, NOW));
    s.store.save(&s.home).unwrap();
    let (code, _, _) = http(
        s.addr,
        "GET",
        "/api/sessions",
        &[("Authorization", &bearer)],
        "",
    )
    .await;
    assert_eq!(code, 401, "撤銷後立即失效，不必重啟");
}

#[tokio::test]
async fn login_sets_a_signed_cookie_that_needs_a_matching_origin() {
    let s = start(AuthMode::Always).await;
    let body = format!("key={}", s.operate);
    let (code, head, _) = http(s.addr, "POST", "/auth/login", &[], &body).await;
    assert_eq!(code, 303, "{head}");
    assert_eq!(header(&head, "location"), Some("/"));
    let sc = header(&head, "set-cookie").unwrap();
    assert!(
        sc.contains("HttpOnly") && sc.contains("SameSite=Strict") && sc.contains("Path=/"),
        "{sc}"
    );
    assert!(
        !sc.contains("Secure"),
        "http 不加 Secure（X-Forwarded-Proto https 才加）"
    );
    let cookie = cookie_of(&head);

    let (code, _, _) = http(s.addr, "GET", "/api/sessions", &[("Cookie", &cookie)], "").await;
    assert_eq!(code, 200);
    let same = format!("http://{}", s.addr);
    let (code, _, _) = http(
        s.addr,
        "GET",
        "/api/sessions",
        &[("Cookie", &cookie), ("Origin", &same)],
        "",
    )
    .await;
    assert_eq!(code, 200);
    let (code, _, _) = http(
        s.addr,
        "GET",
        "/api/sessions",
        &[("Cookie", &cookie), ("Origin", "http://evil.example")],
        "",
    )
    .await;
    assert_eq!(code, 403, "cookie 要配 Origin == Host");

    let mut bad = cookie.clone();
    let last = bad.pop().unwrap();
    bad.push(if last == '0' { '1' } else { '0' });
    let (code, _, _) = http(s.addr, "GET", "/api/sessions", &[("Cookie", &bad)], "").await;
    assert_eq!(code, 401, "竄改的 cookie");

    let (code, head, _) = http(s.addr, "POST", "/auth/login", &[], "key=clk_zzzzzzzz_wrong").await;
    assert_eq!(code, 401, "{head}");
    assert!(header(&head, "set-cookie").is_none());

    let (code, head, _) = http(s.addr, "POST", "/auth/logout", &[("Cookie", &cookie)], "").await;
    assert_eq!(code, 303);
    assert!(header(&head, "set-cookie").unwrap().contains("Max-Age=0"));
}

#[tokio::test]
async fn forwarded_https_marks_the_cookie_secure() {
    let s = start(AuthMode::Always).await;
    let body = format!("key={}", s.view);
    let (code, head, _) = http(
        s.addr,
        "POST",
        "/auth/login",
        &[("X-Forwarded-Proto", "https")],
        &body,
    )
    .await;
    assert_eq!(code, 303);
    assert!(header(&head, "set-cookie").unwrap().contains("Secure"));
}

#[tokio::test]
async fn cookie_survives_restart_and_dies_with_the_key() {
    let mut s = start(AuthMode::Always).await;
    let body = format!("key={}", s.view);
    let (_, head, _) = http(s.addr, "POST", "/auth/login", &[], &body).await;
    let cookie = cookie_of(&head);

    // 「重啟」：同一個 keys.json 開第二個 server
    let state = WebState::new_with_auth(None, None, 8, s.home.clone(), AuthMode::Always);
    let (addr2, _) = serve_web("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap();
    let (code, _, _) = http(addr2, "GET", "/api/sessions", &[("Cookie", &cookie)], "").await;
    assert_eq!(code, 200, "server secret 在檔裡，重啟不必重登");

    let id = keys::parse_token(&s.view).unwrap().0.to_string();
    s.store.revoke(&id, NOW);
    s.store.save(&s.home).unwrap();
    let (code, _, _) = http(addr2, "GET", "/api/sessions", &[("Cookie", &cookie)], "").await;
    assert_eq!(code, 401, "撤銷金鑰，cookie 立即失效");
}

#[tokio::test]
async fn ten_failed_logins_per_minute_then_429() {
    let s = start(AuthMode::Always).await;
    for _ in 0..10 {
        let (code, _, _) = http(s.addr, "POST", "/auth/login", &[], "key=clk_zzzzzzzz_wrong").await;
        assert_eq!(code, 401);
    }
    let (code, _, _) = http(s.addr, "POST", "/auth/login", &[], "key=clk_zzzzzzzz_wrong").await;
    assert_eq!(code, 429);
    let body = format!("key={}", s.view);
    let (code, _, _) = http(s.addr, "POST", "/auth/login", &[], &body).await;
    assert_eq!(code, 429, "被擋期間對的也不收");
}

#[tokio::test]
async fn once_link_logs_in_exactly_once() {
    let s = start(AuthMode::Always).await;
    let id = keys::parse_token(&s.operate).unwrap().0.to_string();
    let code =
        auth::create_once_link(&s.home, &id, coding_lab::now_ms(), &mut keys::test_rng(3)).unwrap();
    let (status, head, _) = http(s.addr, "GET", &format!("/auth/once/{code}"), &[], "").await;
    assert_eq!(status, 303, "{head}");
    let cookie = cookie_of(&head);
    let (status, _, _) = http(s.addr, "GET", "/api/sessions", &[("Cookie", &cookie)], "").await;
    assert_eq!(status, 200);
    let (status, _, _) = http(s.addr, "GET", &format!("/auth/once/{code}"), &[], "").await;
    assert_eq!(status, 404, "用過就沒了");
}

async fn ws_with(
    addr: SocketAddr,
    headers: &[(&str, &str)],
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    for (k, v) in headers {
        let name: tokio_tungstenite::tungstenite::http::HeaderName = k.parse().unwrap();
        req.headers_mut().insert(name, v.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
}

async fn hello(ws: &mut Ws) -> Value {
    ws.send(Message::Text(
        json!({"op": "hello", "protocol": 1}).to_string().into(),
    ))
    .await
    .unwrap();
    loop {
        if let Some(Ok(Message::Text(t))) = ws.next().await {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["t"] == "welcome" {
                return v;
            }
        } else {
            panic!("沒等到 welcome");
        }
    }
}

async fn recv(ws: &mut Ws) -> Value {
    match tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str(&t).unwrap(),
        other => panic!("沒等到訊息：{other:?}"),
    }
}

#[tokio::test]
async fn ws_handshake_needs_a_key_and_view_cannot_ack() {
    let s = start(AuthMode::Always).await;
    assert!(ws_with(s.addr, &[]).await.is_err(), "沒金鑰不建立連線");

    let bearer = format!("Bearer {}", s.view);
    let mut ws = ws_with(s.addr, &[("Authorization", &bearer)])
        .await
        .unwrap();
    let w = hello(&mut ws).await;
    assert_eq!(w["scope"], "view");
    ws.send(Message::Text(
        json!({"op": "ack", "session": "s", "kind": "done", "ref": "r"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let r = recv(&mut ws).await;
    assert_eq!(r["t"], "error", "{r}");
    assert!(r["message"].as_str().unwrap().contains("operate"), "{r}");

    let bearer = format!("Bearer {}", s.operate);
    let mut ws = ws_with(s.addr, &[("Authorization", &bearer)])
        .await
        .unwrap();
    let w = hello(&mut ws).await;
    assert_eq!(w["scope"], "operate");

    // cookie 也能握手，但 Origin 要對
    let body = format!("key={}", s.view);
    let (_, head, _) = http(s.addr, "POST", "/auth/login", &[], &body).await;
    let cookie = cookie_of(&head);
    assert!(
        ws_with(
            s.addr,
            &[("Cookie", &cookie), ("Origin", "http://evil.example")]
        )
        .await
        .is_err()
    );
    let same = format!("http://{}", s.addr);
    assert!(
        ws_with(s.addr, &[("Cookie", &cookie), ("Origin", &same)])
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn local_open_on_loopback_needs_nothing_and_is_scope_local() {
    let s = start(AuthMode::LocalOpen).await;
    let (code, _, _) = http(s.addr, "GET", "/api/sessions", &[], "").await;
    assert_eq!(code, 200, "SSH 通道進來也是 localhost，等同信任");
    let mut ws = ws_with(s.addr, &[]).await.unwrap();
    let w = hello(&mut ws).await;
    assert_eq!(w["scope"], "local");
    // 帶了金鑰也不會變成 view（本機永遠完整）
    let bearer = format!("Bearer {}", s.view);
    let mut ws = ws_with(s.addr, &[("Authorization", &bearer)])
        .await
        .unwrap();
    assert_eq!(hello(&mut ws).await["scope"], "local");
}

#[tokio::test]
async fn without_keys_file_remote_style_auth_always_fails_but_login_page_explains() {
    let home = tmp("nokeys");
    let state = WebState::new_with_auth(None, None, 8, home, AuthMode::Always);
    let (addr, _) = serve_web("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap();
    let (code, _, _) = http(
        addr,
        "GET",
        "/api/sessions",
        &[("Authorization", "Bearer clk_abcdefgh_x")],
        "",
    )
    .await;
    assert_eq!(code, 401);
    let (code, _, body) = http(addr, "GET", "/auth/login", &[], "").await;
    assert_eq!(code, 200);
    assert!(body.contains("key create"), "沒有金鑰時提示怎麼建：{body}");
}

/// 成功驗證會寫回 last_used（最多每分鐘一次），失敗不寫。
#[tokio::test]
async fn successful_auth_records_last_used() {
    let s = start(AuthMode::Always).await;
    let bearer = format!("Bearer {}", s.view);
    let (code, _, _) = http(
        s.addr,
        "GET",
        "/api/sessions",
        &[("Authorization", &bearer)],
        "",
    )
    .await;
    assert_eq!(code, 200);
    let id = keys::parse_token(&s.view).unwrap().0;
    let store = KeyStore::load(&s.home).unwrap().unwrap();
    assert!(store.get(id).unwrap().last_used_ms.is_some());
    let op = keys::parse_token(&s.operate).unwrap().0;
    assert!(store.get(op).unwrap().last_used_ms.is_none());
}

// ── 反向代理（S-1）：loopback 來源帶 X-Forwarded-*／Forwarded 一律當遠端 ─────────

/// 一個有 session 的 store：prompt 裡有 clk_ 金鑰、有一段 thinking（遠端要剝的兩樣東西）。
fn proxied_server_store(
    home: &Path,
    secret: &str,
) -> std::sync::Arc<std::sync::Mutex<coding_lab::store::Store>> {
    let ev = |seq: u64, kind: &str, pos: Option<u64>, payload: Value| {
        coding_lab::event::Event::from_json_line(
            &json!({"v": 1, "session": "s1", "seq": seq, "type": kind, "agent": "main", "line": null,
                    "pos": pos, "ts_model": null, "ts_exec": null, "ts_seen": NOW + seq, "host": "local",
                    "payload": payload})
            .to_string(),
        )
        .unwrap()
    };
    let mut s = coding_lab::store::Store::open(home).unwrap();
    s.append(ev(1, "session.started", None,
        json!({"cwd": "/p", "source": "startup", "model": "m", "transcriptPath": "/t", "branch": "main"})));
    s.append(ev(
        2,
        "prompt.submitted",
        None,
        json!({"text": format!("use {secret} please"), "promptId": "p1"}),
    ));
    s.append(ev(
        3,
        "assistant.thinking",
        Some(1),
        json!({"text": "THINKING-FULL-TEXT", "summarized": false, "messageId": "m", "model": "m"}),
    ));
    s.flush().unwrap();
    std::sync::Arc::new(std::sync::Mutex::new(s))
}

async fn start_proxied() -> Server {
    let home = tmp("proxy");
    let mut store = KeyStore::new_with_secret([9u8; 32]);
    let mut rng = keys::test_rng(42);
    let view = store.create("view", Scope::View, None, NOW, &mut rng);
    let operate = store.create("op", Scope::Operate, None, NOW, &mut rng);
    store.save(&home).unwrap();
    let st = proxied_server_store(&home, &operate);
    let state = WebState::new_with_auth(Some(st), None, 8, home.clone(), AuthMode::LocalOpen);
    state.set_remote_thinking(coding_lab::config::RemoteThinking::Hide);
    let (addr, _) = serve_web("127.0.0.1:0".parse().unwrap(), state)
        .await
        .unwrap();
    Server {
        addr,
        home,
        store,
        view,
        operate,
    }
}

/// nginx 的 `proxy_pass` 預設把 Host 改成 upstream：來源與 Host 都是 loopback，只剩 X-Forwarded-* 看得出是代理轉來的。
#[tokio::test]
async fn proxied_request_on_loopback_needs_a_key() {
    let s = start_proxied().await;
    for fwd in [
        ("X-Forwarded-For", "203.0.113.9"),
        ("X-Forwarded-Host", "lab.example.com"),
        ("X-Forwarded-Proto", "https"),
        ("Forwarded", "for=203.0.113.9;proto=https"),
    ] {
        let (code, _, _) = http(s.addr, "GET", "/api/sessions", &[fwd], "").await;
        assert_eq!(code, 401, "{fwd:?}：代理轉來的請求在 local-open 下也要金鑰");
    }
    // 沒有轉發標頭的本機請求照舊免金鑰
    let (code, _, _) = http(s.addr, "GET", "/api/sessions", &[], "").await;
    assert_eq!(code, 200);
}

#[tokio::test]
async fn proxied_request_with_a_key_is_redacted_like_a_remote_one() {
    let s = start_proxied().await;
    let bearer = format!("Bearer {}", s.view);
    let fwd = [
        ("X-Forwarded-For", "203.0.113.9"),
        ("Authorization", bearer.as_str()),
    ];
    let (code, _, body) = http(s.addr, "GET", "/api/sessions/s1/events", &fwd, "").await;
    assert_eq!(code, 200);
    assert!(
        !body.contains("THINKING-FULL-TEXT"),
        "remote_thinking=hide 時 thinking 全文要 defer：{body}"
    );
    assert!(!body.contains(&s.operate), "clk_ 金鑰要剝：{body}");
    let (code, _, _) = http(s.addr, "GET", "/api/sessions/s1/thinking/3", &fwd, "").await;
    assert_eq!(code, 403, "hide 時遠端讀不到 thinking 全文");
}

#[tokio::test]
async fn proxied_ws_needs_a_key_and_is_not_local() {
    let s = start_proxied().await;
    assert!(
        ws_with(s.addr, &[("X-Forwarded-For", "203.0.113.9")])
            .await
            .is_err(),
        "代理轉來的 WS 沒有金鑰要被拒"
    );
    let bearer = format!("Bearer {}", s.view);
    let mut ws = ws_with(
        s.addr,
        &[
            ("X-Forwarded-For", "203.0.113.9"),
            ("Authorization", &bearer),
        ],
    )
    .await
    .unwrap();
    let w = hello(&mut ws).await;
    assert_eq!(w["scope"], "view");
    assert_eq!(
        w["thinking"], "hide",
        "代理轉來的連線照遠端的 remote_thinking：{w}"
    );
}

// ── 金鑰檔的並發（#743 S-3、S-7、E-H10，X12） ─────────────────────────────────────

/// collector 驗金鑰不再寫 keys.json（last_used 另存一個檔）：keys.json 只有 CLI 的 create／revoke 會寫。
#[test]
fn verifying_a_key_never_rewrites_keys_json_but_last_used_is_kept() {
    let home = tmp("used");
    let mut rng = keys::test_rng(5);
    let mut ks = KeyStore::new(&mut rng);
    let token = ks.create("laptop", Scope::View, None, 1, &mut rng);
    let id = keys::parse_token(&token).unwrap().0.to_string();
    ks.save(&home).unwrap();
    let before = std::fs::read(KeyStore::path(&home)).unwrap();
    let a = auth::Auth::new(home.clone(), AuthMode::Always);
    a.verify_token(&token, NOW).unwrap();
    assert_eq!(
        std::fs::read(KeyStore::path(&home)).unwrap(),
        before,
        "keys.json 原封不動"
    );
    let loaded = KeyStore::load(&home).unwrap().unwrap();
    assert_eq!(
        loaded.get(&id).unwrap().last_used_ms,
        Some(NOW),
        "key list 仍看得到 last_used"
    );
}

/// CLI 撤銷時 collector 正在寫 last_used：撤銷不能被蓋掉，CLI 也不能寫失敗（S-3）。
#[test]
fn revoke_is_never_lost_to_a_concurrent_last_used_write() {
    use std::sync::{Arc, Barrier};
    let (mut lost, mut failed) = (0, 0);
    for i in 0..50u64 {
        let home = tmp("revoke-race");
        let mut rng = keys::test_rng(i + 3);
        let mut ks = KeyStore::new(&mut rng);
        let token = ks.create("laptop", Scope::View, None, 1, &mut rng);
        let id = keys::parse_token(&token).unwrap().0.to_string();
        ks.save(&home).unwrap();
        let a = Arc::new(auth::Auth::new(home.clone(), AuthMode::Always));
        let b = Arc::new(Barrier::new(2));
        let (a2, b2, tok) = (a.clone(), b.clone(), token.clone());
        let h = std::thread::spawn(move || {
            b2.wait();
            for k in 0..20u64 {
                let _ = a2.verify_token(&tok, NOW + k * 61_000);
            }
        });
        b.wait();
        let mut cli = KeyStore::load(&home).unwrap().unwrap();
        cli.revoke(&id, 5);
        let saved = cli.save(&home).is_ok();
        h.join().unwrap();
        if !saved {
            failed += 1;
        } else if KeyStore::load(&home)
            .unwrap()
            .unwrap()
            .get(&id)
            .unwrap()
            .revoked_ms
            .is_none()
        {
            lost += 1;
        }
        assert!(
            a.verify_token(&token, NOW + 2_000_000).is_err() || !saved,
            "撤銷之後不能再用"
        );
    }
    assert_eq!(
        (lost, failed),
        (0, 0),
        "撤銷被蓋掉 {lost} 次、CLI 寫入失敗 {failed} 次"
    );
}

/// 同一個一次性連結被同時兌換：最多一次成功（S-7）。
#[test]
fn once_link_is_redeemed_at_most_once_under_concurrency() {
    use std::sync::{Arc, Barrier};
    for i in 0..200u64 {
        let home = tmp("once-race");
        let code =
            auth::create_once_link(&home, "abcdefgh", NOW, &mut keys::test_rng(i + 1)).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let (home, code, b) = (home.clone(), code.clone(), barrier.clone());
                std::thread::spawn(move || {
                    b.wait();
                    auth::consume_once_link(&home, &code, NOW + 1)
                })
            })
            .collect();
        let ok = hs
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|r| matches!(r, Ok(Some(_))))
            .count();
        assert!(ok <= 1, "第 {i} 輪同一個 code 兌換成功 {ok} 次");
        assert_eq!(ok, 1, "第 {i} 輪：要有一次成功");
    }
}
