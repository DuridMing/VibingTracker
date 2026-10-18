//! 金鑰管理（#666，PLAN「網路綁定」第 4 點）。
//!
//! 金鑰長 `clk_<8 碼 id>_<32 bytes 的 base62>`，只在建立時印出一次；`<home>/keys.json`（權限 600）只存
//! id、名稱、scope、secret 的 SHA-256、建立／最後使用／到期／撤銷時間，以及簽 cookie 用的 server secret。
//! **不放 SQLite**：SQLite 可以從 JSONL 重建，金鑰不行。
//!
//! 時間一律由呼叫端注入（epoch ms），隨機來源以 [`Rng`] 注入，測試可重現。

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const FILE_NAME: &str = "keys.json";
/// 每把金鑰最後一次用到的時間（collector 寫）。和 `keys.json` 分開：`keys.json` 只有 CLI 的 create／revoke 會寫，
/// collector 寫回 last_used 就不會蓋掉剛撤銷的金鑰（#743 S-3）。
pub const USED_FILE: &str = "keys-usage.json";
pub const TOKEN_PREFIX: &str = "clk_";
pub const ID_LEN: usize = 8;
/// 32 bytes 以 base62 表示固定補到 43 個字元（⌈256 / log2 62⌉）。
pub const SECRET_LEN: usize = 43;
const FILE_VERSION: u32 = 1;
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// 金鑰能做什麼。`view`：看串流、Sessions、檔案、diff、點開 thinking；`operate`：另外可按「已確認」、改通知設定。
/// 沒有任何 scope 能操作 Claude Code 本身。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    View,
    Operate,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::View => "view",
            Scope::Operate => "operate",
        }
    }
}

impl std::str::FromStr for Scope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "view" => Ok(Scope::View),
            "operate" => Ok(Scope::Operate),
            _ => Err(format!("scope 只能是 view 或 operate，收到 {s}")),
        }
    }
}

/// `keys.json` 裡的一把金鑰（沒有明文 secret）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRecord {
    pub id: String,
    pub name: String,
    pub scope: Scope,
    /// secret 的 SHA-256，小寫十六進位
    pub secret_sha256: String,
    pub created_ms: u64,
    #[serde(default)]
    pub last_used_ms: Option<u64>,
    #[serde(default)]
    pub expires_ms: Option<u64>,
    #[serde(default)]
    pub revoked_ms: Option<u64>,
}

impl KeyRecord {
    /// 現在（`now_ms`）還能不能用；回 Err 說明為什麼不能。
    pub fn usable_at(&self, now_ms: u64) -> Result<(), VerifyError> {
        if self.revoked_ms.is_some() {
            return Err(VerifyError::Revoked);
        }
        if self.expires_ms.is_some_and(|e| now_ms >= e) {
            return Err(VerifyError::Expired);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// 不是 `clk_<id>_<secret>` 的形狀
    Malformed,
    UnknownKey,
    BadSecret,
    Revoked,
    Expired,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            VerifyError::Malformed => "金鑰格式不對",
            VerifyError::UnknownKey => "沒有這把金鑰",
            VerifyError::BadSecret => "金鑰不符",
            VerifyError::Revoked => "金鑰已撤銷",
            VerifyError::Expired => "金鑰已到期",
        })
    }
}

/// 隨機來源；正式用 [`SystemRng`]，測試用 [`test_rng`]。
pub trait Rng {
    fn fill(&mut self, buf: &mut [u8]);
}

/// 作業系統的亂數（`getrandom`）。
pub struct SystemRng;

impl Rng for SystemRng {
    fn fill(&mut self, buf: &mut [u8]) {
        // getrandom 只在系統亂數來源壞掉時失敗；那時寧可 panic 也不要發出可預測的金鑰
        getrandom::fill(buf).expect("系統亂數來源不可用");
    }
}

/// 可重現的假亂數（xorshift64*），只給測試用。
pub struct TestRng(u64);

pub fn test_rng(seed: u64) -> TestRng {
    TestRng(seed.max(1))
}

impl Rng for TestRng {
    fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            *b = (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8;
        }
    }
}

#[derive(Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    /// 簽 cookie 用（#667），32 bytes 十六進位
    server_secret: String,
    keys: Vec<KeyRecord>,
}

/// 記憶體裡的 `keys.json`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyStore {
    server_secret: [u8; 32],
    keys: Vec<KeyRecord>,
}

impl KeyStore {
    /// 新的空檔，server secret 由 `rng` 產生。
    pub fn new(rng: &mut dyn Rng) -> KeyStore {
        let mut s = [0u8; 32];
        rng.fill(&mut s);
        KeyStore::new_with_secret(s)
    }

    pub fn new_with_secret(server_secret: [u8; 32]) -> KeyStore {
        KeyStore {
            server_secret,
            keys: Vec::new(),
        }
    }

    pub fn server_secret(&self) -> &[u8; 32] {
        &self.server_secret
    }

    pub fn list(&self) -> &[KeyRecord] {
        &self.keys
    }

    pub fn get(&self, id: &str) -> Option<&KeyRecord> {
        self.keys.iter().find(|k| k.id == id)
    }

    /// 建一把金鑰，回傳**唯一一次**可拿到的明文 token。
    pub fn create(
        &mut self,
        name: &str,
        scope: Scope,
        expires_ms: Option<u64>,
        now_ms: u64,
        rng: &mut dyn Rng,
    ) -> String {
        let id = loop {
            let mut raw = [0u8; ID_LEN];
            rng.fill(&mut raw);
            let id: String = raw
                .iter()
                .map(|b| ALPHABET[(*b % 62) as usize] as char)
                .collect();
            if self.get(&id).is_none() {
                break id;
            }
        };
        let secret = random_base62(rng);
        self.keys.push(KeyRecord {
            id: id.clone(),
            name: name.to_string(),
            scope,
            secret_sha256: sha256_hex(secret.as_bytes()),
            created_ms: now_ms,
            last_used_ms: None,
            expires_ms,
            revoked_ms: None,
        });
        format!("{TOKEN_PREFIX}{id}_{secret}")
    }

    /// 驗一個完整的 token；成功時更新 `last_used_ms` 並回那把金鑰。secret 以常數時間比對。
    pub fn verify(&mut self, token: &str, now_ms: u64) -> Result<&KeyRecord, VerifyError> {
        let (id, secret) = parse_token(token).ok_or(VerifyError::Malformed)?;
        let i = self
            .keys
            .iter()
            .position(|k| k.id == id)
            .ok_or(VerifyError::UnknownKey)?;
        let given = sha256_hex(secret.as_bytes());
        if !ct_eq(given.as_bytes(), self.keys[i].secret_sha256.as_bytes()) {
            return Err(VerifyError::BadSecret);
        }
        self.keys[i].usable_at(now_ms)?;
        self.keys[i].last_used_ms = Some(now_ms);
        Ok(&self.keys[i])
    }

    /// 撤銷；第一次回 true，已撤銷或不存在回 false（保留第一次的時間）。
    pub fn revoke(&mut self, id: &str, now_ms: u64) -> bool {
        match self.keys.iter_mut().find(|k| k.id == id) {
            Some(k) if k.revoked_ms.is_none() => {
                k.revoked_ms = Some(now_ms);
                true
            }
            _ => false,
        }
    }

    pub fn path(home: &Path) -> PathBuf {
        home.join(FILE_NAME)
    }

    /// 讀 `<home>/keys.json`；檔案不存在回 `Ok(None)`。unix 上權限不是 600 時拒絕並說怎麼修。
    pub fn load(home: &Path) -> Result<Option<KeyStore>, String> {
        let path = KeyStore::path(home);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("讀不到 {}：{e}", path.display())),
        };
        check_mode(&path)?;
        let f: FileFormat =
            serde_json::from_str(&text).map_err(|e| format!("{} 格式錯誤：{e}", path.display()))?;
        if f.version > FILE_VERSION {
            return Err(format!(
                "{} 是版本 {}，這個 coding-lab 只認得到版本 {FILE_VERSION}；請升級",
                path.display(),
                f.version
            ));
        }
        let server_secret = hex_decode32(&f.server_secret)
            .ok_or_else(|| format!("{} 的 server_secret 不是 32 bytes 十六進位", path.display()))?;
        let mut keys = f.keys;
        // last_used 另存在 keys-usage.json；兩邊取較晚的（舊版 collector 寫在 keys.json 裡的也算）
        let used = read_used(home);
        for k in &mut keys {
            if let Some(t) = used.get(&k.id) {
                k.last_used_ms = Some(k.last_used_ms.map_or(*t, |o| o.max(*t)));
            }
        }
        Ok(Some(KeyStore {
            server_secret,
            keys,
        }))
    }

    /// 寫回 `<home>/keys.json`：先寫 `.tmp`（600）再 rename，不會留下半個檔。
    pub fn save(&self, home: &Path) -> Result<(), String> {
        std::fs::create_dir_all(home).map_err(|e| format!("建不了 {}：{e}", home.display()))?;
        let path = KeyStore::path(home);
        let f = FileFormat {
            version: FILE_VERSION,
            server_secret: hex_encode(&self.server_secret),
            keys: self.keys.clone(),
        };
        let text = serde_json::to_string_pretty(&f).map_err(|e| e.to_string())?;
        save_private_json(&path, &text)
    }
}

fn read_used(home: &Path) -> std::collections::HashMap<String, u64> {
    std::fs::read_to_string(home.join(USED_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// 記下這把金鑰在 `now_ms` 用過（寫 `keys-usage.json`，不碰 `keys.json`）。同一個程序內依序寫，不會互相蓋掉。
pub fn record_used(home: &Path, id: &str, now_ms: u64) -> Result<(), String> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut used = read_used(home);
    let t = used.entry(id.to_string()).or_insert(0);
    *t = (*t).max(now_ms);
    let text = serde_json::to_string(&used).map_err(|e| e.to_string())?;
    save_private_json(&home.join(USED_FILE), &text)
}

/// 寫一個只有自己能讀的 JSON 檔：先寫 `<path>.tmp`（unix 600）再 rename，不會留下半個檔。
pub(crate) fn save_private_json(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, text.as_bytes()).map_err(|e| format!("寫不了 {}：{e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("寫不了 {}：{e}", path.display())
    })
}

/// 32 bytes 的亂數，以 43 碼 base62 表示（金鑰的 secret、一次性登入連結的 code）。
pub(crate) fn random_base62(rng: &mut dyn Rng) -> String {
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes);
    base62_fixed(&bytes)
}

/// 拆 `clk_<id>_<secret>`；形狀不對回 None（不修剪空白）。
pub fn parse_token(token: &str) -> Option<(&str, &str)> {
    let rest = token.strip_prefix(TOKEN_PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    let base62 = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric());
    (id.len() == ID_LEN && base62(id) && base62(secret)).then_some((id, secret))
}

/// 常數時間比對：長度不同直接不相等，相同時看完每個 byte 才回答。
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(data).into();
    hex_encode(&digest)
}

/// `90d`／`12h`／`30m`／`never` → 到期時間（epoch ms）。
pub fn parse_expires(s: &str, now_ms: u64) -> Result<Option<u64>, String> {
    if s == "never" {
        return Ok(None);
    }
    let bad = || format!("--expires 要像 90d、12h、30m 或 never，收到 {s}");
    // 單位是最後一個字元；用字元切，不用位元組（「9天」切在「天」中間會 panic，W-L1）
    let (num, unit) = s
        .char_indices()
        .last()
        .map(|(i, _)| s.split_at(i))
        .ok_or_else(bad)?;
    let n: u64 = num.parse().map_err(|_| bad())?;
    if n == 0 {
        return Err(bad());
    }
    let unit_ms: u64 = match unit {
        "d" => 24 * 60 * 60 * 1000,
        "h" => 60 * 60 * 1000,
        "m" => 60 * 1000,
        _ => return Err(bad()),
    };
    n.checked_mul(unit_ms)
        .and_then(|ms| now_ms.checked_add(ms))
        .map(Some)
        .ok_or_else(|| format!("--expires 太長：{s}"))
}

/// epoch ms → `YYYY-MM-DD HH:MM UTC`（list 用；不拉日期 crate）。
pub fn fmt_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant 的 civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60
    )
}

fn base62_fixed(bytes: &[u8; 32]) -> String {
    // 大數除以 62 取餘；32 bytes 最多 43 位，不足補 '0'
    let mut num = bytes.to_vec();
    let mut digits = Vec::with_capacity(SECRET_LEN);
    while num.iter().any(|&b| b != 0) {
        let mut rem = 0u32;
        for b in num.iter_mut() {
            let cur = (rem << 8) | *b as u32;
            *b = (cur / 62) as u8;
            rem = cur % 62;
        }
        digits.push(ALPHABET[rem as usize]);
    }
    while digits.len() < SECRET_LEN {
        digits.push(b'0');
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(out)
}

#[cfg(unix)]
fn check_mode(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("讀不到 {}：{e}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} 的權限是 {mode:o}，必須是 600（只有你自己能讀）：chmod 600 {}",
            path.display(),
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_mode(_path: &Path) -> Result<(), String> {
    // Windows 的 ACL 沒有對應的簡單檢查；%USERPROFILE% 底下預設只有自己能讀
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // 檔案已存在時 mode 不會被套用，再設一次
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(data)?;
    f.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}
