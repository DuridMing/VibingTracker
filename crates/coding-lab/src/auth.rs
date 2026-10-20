//! web 入口的驗證（#667，PLAN「網路綁定」第 4 點，contracts.md §6–§7）。
//!
//! 遠端連線（來源不是 loopback）或 `[web].auth = "always"` 時，每個請求都要帶登入 cookie 或
//! `Authorization: Bearer clk_…`。cookie 是以 `keys.json` 的 server secret 做 HMAC-SHA256 簽的
//! `<key id>.<到期 ms>.<簽章>`：collector 重啟不必重登，但每次仍比對 `keys.json`，撤銷立即生效。
//! hook 入口（4800）完全不經過這裡（不變式 6）。
//!
//! 時間一律注入（epoch ms）；`keys.json` 換過（mtime、大小或 inode 變了）就重讀，CLI 改檔不必重啟。

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use axum::http::{HeaderMap, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::AuthMode;
use crate::keys::{self, KeyStore, Rng, Scope};

pub const COOKIE_NAME: &str = "coding_lab_session";
pub const COOKIE_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// 一次性登入連結的壽命
pub const ONCE_TTL_MS: u64 = 5 * 60 * 1000;
pub const LOGIN_FAILS_PER_MINUTE: usize = 10;
pub const LINKS_FILE: &str = "login-links.json";
/// 成功驗證後 `last_used_ms` 最多多久寫回 `keys.json` 一次（每個請求都寫太吵）
pub const LAST_USED_FLUSH_MS: u64 = 60_000;

// ── HMAC 與 cookie ────────────────────────────────────────────────────────

/// HMAC-SHA256（RFC 2104）；只用在 cookie 簽章，不另拉 crate。
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let d: [u8; 32] = Sha256::digest(key).into();
        k[..32].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(data);
    let ih: [u8; 32] = inner.finalize().into();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(ih);
    outer.finalize().into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CookieError {
    Malformed,
    BadSignature,
    Expired,
}

fn cookie_mac(secret: &[u8; 32], key_id: &str, expires_ms: u64) -> String {
    hmac_sha256(secret, format!("{key_id}.{expires_ms}").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `<key id>.<到期 ms>.<HMAC 十六進位>`。
pub fn sign_cookie(secret: &[u8; 32], key_id: &str, expires_ms: u64) -> String {
    format!(
        "{key_id}.{expires_ms}.{}",
        cookie_mac(secret, key_id, expires_ms)
    )
}

/// 驗簽章再看到期（`now >= expires` 就不算）；回 (key id, 到期)。
pub fn parse_cookie(
    secret: &[u8; 32],
    value: &str,
    now_ms: u64,
) -> Result<(String, u64), CookieError> {
    let mut it = value.splitn(3, '.');
    let (Some(id), Some(exp), Some(mac)) = (it.next(), it.next(), it.next()) else {
        return Err(CookieError::Malformed);
    };
    if id.is_empty() || mac.is_empty() {
        return Err(CookieError::Malformed);
    }
    let exp: u64 = exp.parse().map_err(|_| CookieError::Malformed)?;
    if !keys::ct_eq(mac.as_bytes(), cookie_mac(secret, id, exp).as_bytes()) {
        return Err(CookieError::BadSignature);
    }
    if now_ms >= exp {
        return Err(CookieError::Expired);
    }
    Ok((id.to_string(), exp))
}

/// `Set-Cookie` 的值；`max_age_s = 0` 就是清掉。`secure` 在前面有 TLS（`X-Forwarded-Proto: https`）時才加。
pub fn set_cookie_header(value: &str, max_age_s: u64, secure: bool) -> String {
    let mut s =
        format!("{COOKIE_NAME}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_s}");
    if secure {
        s.push_str("; Secure");
    }
    s
}

/// 請求裡的登入 cookie（有多個 `Cookie` 標頭也找得到）。
pub fn cookie_value(headers: &HeaderMap) -> Option<String> {
    cookie_named(headers, COOKIE_NAME)
}

/// 請求裡名為 `name` 的 cookie（有多個 `Cookie` 標頭也找得到）。
pub fn cookie_named(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .map(str::trim)
        .find_map(|kv| kv.strip_prefix(name).and_then(|r| r.strip_prefix('=')))
        .map(str::to_string)
}

/// 前面有 TLS 終結（Caddy）時瀏覽器是 https，cookie 要加 `Secure`。
pub fn forwarded_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"))
}

/// `Origin` 必須和 `Host` 相同（沒帶 `Origin` 的非瀏覽器程式放行）。
pub fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or("");
    !host.is_empty() && authority.trim_end_matches('/') == host
}

/// `application/x-www-form-urlencoded` 裡的一個欄位。
pub fn form_value(body: &str, name: &str) -> Option<String> {
    body.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == name).then(|| crate::http_util::form_decode(v))
    })
}

// ── 一次性登入連結 ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct OnceLink {
    code_sha256: String,
    key_id: String,
    expires_ms: u64,
}

fn read_links(home: &Path) -> Result<Vec<OnceLink>, String> {
    let path = home.join(LINKS_FILE);
    match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| keys::corrupt(&path, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("讀不到 {}：{e}", path.display())),
    }
}

fn write_links(home: &Path, links: &[OnceLink]) -> Result<(), String> {
    crate::store::create_private_dir(home)
        .map_err(|e| format!("建不了 {}：{e}", home.display()))?;
    let text = serde_json::to_string_pretty(links).map_err(|e| e.to_string())?;
    keys::save_secret_json_with_backup(&home.join(LINKS_FILE), &text)
}

/// 建一個 5 分鐘內、只能用一次的登入連結 code（`coding-lab key login-link`）。檔裡只存 code 的 SHA-256。
pub fn create_once_link(
    home: &Path,
    key_id: &str,
    now_ms: u64,
    rng: &mut dyn Rng,
) -> Result<String, String> {
    let code = keys::random_base62(rng);
    // 讀改寫在 keys.lock 裡（跨程序，W-L13）：同時建的連結不會互相蓋掉
    let _lock = keys::lock_home(home)?;
    let mut links = read_links(home)?;
    links.retain(|l| l.expires_ms > now_ms);
    links.push(OnceLink {
        code_sha256: keys::sha256_hex(code.as_bytes()),
        key_id: key_id.to_string(),
        expires_ms: now_ms + ONCE_TTL_MS,
    });
    write_links(home, &links)?;
    Ok(code)
}

/// 用掉一個 code：找得到而且沒過期回它的 key id；不論結果都把它從檔裡拿掉。
pub fn consume_once_link(home: &Path, code: &str, now_ms: u64) -> Result<Option<String>, String> {
    // 同時兩個兌換不會都讀到同一個 code（S-7），也不會蓋掉 CLI 剛建的連結（W-L13）
    let _lock = keys::lock_home(home)?;
    let mut links = read_links(home)?;
    if links.is_empty() {
        return Ok(None);
    }
    let digest = keys::sha256_hex(code.as_bytes());
    let hit = links
        .iter()
        .position(|l| keys::ct_eq(l.code_sha256.as_bytes(), digest.as_bytes()));
    let found = hit.map(|i| links.remove(i));
    links.retain(|l| l.expires_ms > now_ms);
    write_links(home, &links)?;
    Ok(found.filter(|l| l.expires_ms > now_ms).map(|l| l.key_id))
}

// ── 速率限制 ──────────────────────────────────────────────────────────────

/// 限流表最多記幾個 IP（E-H11）
const RATE_TABLE_MAX: usize = 1024;

/// 每個來源 IP 在 `window_ms` 內最多 `max` 次失敗；超過就暫時不收登入。
pub struct RateLimiter {
    max: usize,
    window_ms: u64,
    hits: HashMap<IpAddr, VecDeque<u64>>,
}

impl RateLimiter {
    pub fn new(max: usize, window_ms: u64) -> RateLimiter {
        RateLimiter {
            max,
            window_ms,
            hits: HashMap::new(),
        }
    }

    /// 表裡記著幾個 IP（測試用）。
    pub fn tracked(&self) -> usize {
        self.hits.len()
    }

    pub fn allowed(&mut self, ip: IpAddr, now_ms: u64) -> bool {
        self.prune(ip, now_ms);
        self.hits.get(&ip).is_none_or(|q| q.len() < self.max)
    }

    pub fn record_failure(&mut self, ip: IpAddr, now_ms: u64) {
        self.prune(ip, now_ms);
        self.hits.entry(ip).or_default().push_back(now_ms);
        // 表滿了（E-H11）：先清掉整個窗口都沒再失敗的 IP，還太多就丟最久沒失敗的。
        // 只清這次請求的 IP 時，不會再來的 IP 永遠留著，換 IPv6 位址就能讓表一直長大
        if self.hits.len() > RATE_TABLE_MAX {
            let window = self.window_ms;
            self.hits
                .retain(|_, q| q.back().is_some_and(|t| t + window > now_ms));
            while self.hits.len() > RATE_TABLE_MAX {
                let Some(oldest) = self
                    .hits
                    .iter()
                    .min_by_key(|(_, q)| q.back().copied().unwrap_or(0))
                    .map(|(ip, _)| *ip)
                else {
                    break;
                };
                self.hits.remove(&oldest);
            }
        }
    }

    fn prune(&mut self, ip: IpAddr, now_ms: u64) {
        let window = self.window_ms;
        if let Some(q) = self.hits.get_mut(&ip) {
            while q.front().is_some_and(|t| t + window <= now_ms) {
                q.pop_front();
            }
            if q.is_empty() {
                self.hits.remove(&ip);
            }
        }
    }
}

// ── 連線的身分 ────────────────────────────────────────────────────────────

/// 一個請求或 WS 連線是誰：本機（免金鑰，什麼都能做）或某把金鑰。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    Local,
    Key { id: String, scope: Scope },
}

impl Identity {
    /// `welcome.scope`：`local`／`view`／`operate`
    pub fn scope_str(&self) -> &'static str {
        match self {
            Identity::Local => "local",
            Identity::Key { scope, .. } => scope.as_str(),
        }
    }

    /// 能不能按「已確認」、改通知設定
    pub fn can_operate(&self) -> bool {
        !matches!(
            self,
            Identity::Key {
                scope: Scope::View,
                ..
            }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Denied {
    /// 沒帶任何憑證
    NoCredentials,
    /// 帶了但不對（原因給 log，不回給客戶端）
    BadCredentials(String),
    /// 用 cookie 時 `Origin` 和 `Host` 不符
    OriginMismatch,
}

/// `keys.json` 有沒有換過：mtime 加上大小與 inode。只看 mtime 時，CLI 在同一個 mtime 刻度內撤銷會讀不到（S-3）；
/// CLI 寫檔是寫暫存檔再 rename，每次都換 inode。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    mtime: Option<SystemTime>,
    len: u64,
    ino: u64,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&m);
    #[cfg(not(unix))]
    let ino = 0;
    Some(FileStamp {
        mtime: m.modified().ok(),
        len: m.len(),
        ino,
    })
}

struct KeysCache {
    mtime: Option<FileStamp>,
    store: Option<KeyStore>,
    /// 每把金鑰上次寫回 last_used 的時間
    flushed: HashMap<String, u64>,
}

/// web 入口的驗證狀態（放在 `WebState` 裡）。
pub struct Auth {
    home: Option<PathBuf>,
    mode: AuthMode,
    keys: Mutex<KeysCache>,
    limiter: Mutex<RateLimiter>,
}

impl Auth {
    /// 沒有資料目錄（測試）：只信本機，遠端一律拒絕。
    pub fn local_only() -> Auth {
        Auth::build(None, AuthMode::LocalOpen)
    }

    pub fn new(home: PathBuf, mode: AuthMode) -> Auth {
        Auth::build(Some(home), mode)
    }

    fn build(home: Option<PathBuf>, mode: AuthMode) -> Auth {
        Auth {
            home,
            mode,
            keys: Mutex::new(KeysCache {
                mtime: None,
                store: None,
                flushed: HashMap::new(),
            }),
            limiter: Mutex::new(RateLimiter::new(LOGIN_FAILS_PER_MINUTE, 60_000)),
        }
    }

    /// 這個來源的連線要不要金鑰：不是 loopback，或 `auth = "always"`。
    pub fn needs_auth(&self, peer: SocketAddr) -> bool {
        !peer.ip().is_loopback() || self.mode == AuthMode::Always
    }

    /// 有沒有任何一把（含已撤銷）金鑰。
    pub fn has_keys(&self) -> bool {
        self.with_keys(|c, _| c.store.as_ref().is_some_and(|s| !s.list().is_empty()))
            .unwrap_or(false)
    }

    /// 拿目前的 `keys.json`（換過就重讀，見 [`FileStamp`]）；沒有資料目錄回 None。讀不到或權限不對時當成沒有金鑰並寫 log。
    fn with_keys<R>(&self, f: impl FnOnce(&mut KeysCache, &Path) -> R) -> Option<R> {
        let home = self.home.as_ref()?;
        let mut cache = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let path = KeyStore::path(home);
        let mtime = file_stamp(&path);
        if mtime != cache.mtime || (mtime.is_some() && cache.store.is_none()) {
            cache.store = match KeyStore::load(home) {
                Ok(s) => s,
                Err(e) => {
                    crate::logging::log(&format!("web：{e}；在修好之前所有金鑰都無效"));
                    None
                }
            };
            cache.mtime = mtime;
        }
        Some(f(&mut cache, home))
    }

    /// 驗一個完整的金鑰；成功回 (id, scope) 並記下 last_used（每把最多每分鐘一次）。
    /// last_used 寫在 `keys-usage.json`、在金鑰快取的鎖外寫：collector 從不寫 `keys.json`，CLI 的撤銷不會被蓋掉
    /// （S-3），驗證也不用等寫檔與 sync（E-H10）。
    pub fn verify_token(&self, token: &str, now_ms: u64) -> Result<(String, Scope), String> {
        let verified = self
            .with_keys(|cache, _| {
                let store = cache
                    .store
                    .as_mut()
                    .ok_or_else(|| "還沒有任何金鑰".to_string())?;
                let (id, scope) = store
                    .verify(token, now_ms)
                    .map(|k| (k.id.clone(), k.scope))
                    .map_err(|e| e.to_string())?;
                let due = cache
                    .flushed
                    .get(&id)
                    .is_none_or(|t| now_ms.saturating_sub(*t) >= LAST_USED_FLUSH_MS);
                if due {
                    cache.flushed.insert(id.clone(), now_ms);
                }
                Ok((id, scope, due))
            })
            .unwrap_or_else(|| Err("這個 collector 沒有金鑰檔".to_string()));
        let (id, scope, due) = verified?;
        if due
            && let Some(home) = &self.home
            && let Err(e) = keys::record_used(home, &id, now_ms)
        {
            crate::logging::log(&format!("web：記下 last_used 失敗：{e}"));
        }
        Ok((id, scope))
    }

    /// 這把 id 現在能不能用（cookie 用）。
    pub fn key_usable(&self, id: &str, now_ms: u64) -> Result<Scope, String> {
        self.with_keys(|cache, _| {
            let store = cache
                .store
                .as_ref()
                .ok_or_else(|| "還沒有任何金鑰".to_string())?;
            let k = store.get(id).ok_or_else(|| "沒有這把金鑰".to_string())?;
            k.usable_at(now_ms).map_err(|e| e.to_string())?;
            Ok(k.scope)
        })
        .unwrap_or_else(|| Err("這個 collector 沒有金鑰檔".to_string()))
    }

    fn server_secret(&self) -> Option<[u8; 32]> {
        self.with_keys(|cache, _| cache.store.as_ref().map(|s| *s.server_secret()))
            .flatten()
    }

    /// 給這把金鑰簽一個 30 天的 cookie（登入成功、一次性連結）。
    pub fn cookie_for(&self, key_id: &str, now_ms: u64) -> Option<String> {
        let secret = self.server_secret()?;
        Some(sign_cookie(&secret, key_id, now_ms + COOKIE_TTL_MS))
    }

    /// 貼金鑰登入：驗金鑰、發 cookie。回 (cookie 值, id, scope)。
    pub fn login(&self, token: &str, now_ms: u64) -> Result<(String, String, Scope), String> {
        let (id, scope) = self.verify_token(token, now_ms)?;
        let cookie = self
            .cookie_for(&id, now_ms)
            .ok_or_else(|| "沒有 server secret".to_string())?;
        Ok((cookie, id, scope))
    }

    /// 從請求標頭認人：先 Bearer、再 cookie（cookie 要配 `Origin == Host`）。本機免金鑰的情況由呼叫端判斷，不經過這裡。
    pub fn identify(&self, headers: &HeaderMap, now_ms: u64) -> Result<Identity, Denied> {
        if let Some(bearer) = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        {
            return self
                .verify_token(bearer.trim(), now_ms)
                .map(|(id, scope)| Identity::Key { id, scope })
                .map_err(Denied::BadCredentials);
        }
        if let Some(cookie) = cookie_value(headers) {
            let secret = self
                .server_secret()
                .ok_or_else(|| Denied::BadCredentials("沒有金鑰檔".into()))?;
            let (id, _) = parse_cookie(&secret, &cookie, now_ms)
                .map_err(|e| Denied::BadCredentials(format!("cookie {e:?}")))?;
            let scope = self
                .key_usable(&id, now_ms)
                .map_err(Denied::BadCredentials)?;
            if !same_origin(headers) {
                return Err(Denied::OriginMismatch);
            }
            return Ok(Identity::Key { id, scope });
        }
        Err(Denied::NoCredentials)
    }

    pub fn login_allowed(&self, ip: IpAddr, now_ms: u64) -> bool {
        self.limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .allowed(ip, now_ms)
    }

    pub fn login_failed(&self, ip: IpAddr, now_ms: u64) {
        self.limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record_failure(ip, now_ms);
    }

    /// 一次性登入連結：用掉 code、發 cookie。回 (cookie 值, key id)。
    pub fn login_once(&self, code: &str, now_ms: u64) -> Result<(String, String), String> {
        let home = self
            .home
            .as_ref()
            .ok_or_else(|| "這個 collector 沒有資料目錄".to_string())?;
        let id =
            consume_once_link(home, code, now_ms)?.ok_or_else(|| "連結已失效或用過".to_string())?;
        self.key_usable(&id, now_ms)?;
        let cookie = self
            .cookie_for(&id, now_ms)
            .ok_or_else(|| "沒有 server secret".to_string())?;
        Ok((cookie, id))
    }
}
