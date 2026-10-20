//! 金鑰管理（#666）：格式與解析、雜湊比對、到期與撤銷、keys.json 的權限與讀寫。
//! 時間全部注入（`now_ms`），不 sleep；隨機以固定 bytes 注入，測試可重現。

use std::path::PathBuf;

use coding_lab::keys::{self, KeyStore, Scope, VerifyError};

fn tmp(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("keys")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const NOW: u64 = 1_760_000_000_000;
const DAY: u64 = 24 * 60 * 60 * 1000;

fn store_with_one_key(name: &str, scope: Scope, expires: Option<u64>) -> (KeyStore, String) {
    let mut s = KeyStore::new_with_secret([7u8; 32]);
    let token = s.create(name, scope, expires, NOW, &mut keys::test_rng(1));
    (s, token)
}

// ── 格式與解析 ──────────────────────────────────────────────────────────────

#[test]
fn token_has_prefix_id_and_base62_secret() {
    let (_, token) = store_with_one_key("laptop", Scope::View, None);
    let parts: Vec<&str> = token.split('_').collect();
    assert_eq!(parts.len(), 3, "clk_<id>_<secret>：{token}");
    assert_eq!(parts[0], "clk");
    assert_eq!(parts[1].len(), 8);
    assert!(parts[1].chars().all(|c| c.is_ascii_alphanumeric()));
    // 32 bytes 的 base62 是 43 個字元
    assert_eq!(parts[2].len(), 43, "{token}");
    assert!(parts[2].chars().all(|c| c.is_ascii_alphanumeric()));
}

#[test]
fn parse_rejects_malformed_tokens() {
    assert!(keys::parse_token("clk_abcdefgh_SECRET").is_some());
    assert!(
        keys::parse_token("abc_abcdefgh_SECRET").is_none(),
        "前綴不對"
    );
    assert!(
        keys::parse_token("clk_abcdefg_SECRET").is_none(),
        "id 不是 8 碼"
    );
    assert!(keys::parse_token("clk_abcdefgh_").is_none(), "沒有 secret");
    assert!(keys::parse_token("clk_abcdefgh").is_none(), "少一段");
    assert!(
        keys::parse_token("clk_abcd-fgh_SECRET").is_none(),
        "id 含非 base62"
    );
    assert!(
        keys::parse_token(" clk_abcdefgh_SECRET").is_none(),
        "前後空白不自動修"
    );
}

#[test]
fn two_keys_get_different_ids_and_secrets() {
    let mut s = KeyStore::new_with_secret([7u8; 32]);
    let mut rng = keys::test_rng(9);
    let a = s.create("a", Scope::View, None, NOW, &mut rng);
    let b = s.create("b", Scope::View, None, NOW, &mut rng);
    assert_ne!(a, b);
    let ids: Vec<_> = s.list().iter().map(|k| k.id.clone()).collect();
    assert_ne!(ids[0], ids[1]);
}

// ── 驗證 ───────────────────────────────────────────────────────────────────

#[test]
fn verify_accepts_the_printed_token_and_records_last_used() {
    let (mut s, token) = store_with_one_key("laptop", Scope::Operate, None);
    let k = s.verify(&token, NOW + 1000).expect("剛建立的金鑰要能用");
    assert_eq!(k.name, "laptop");
    assert_eq!(k.scope, Scope::Operate);
    assert_eq!(k.last_used_ms, Some(NOW + 1000));
}

#[test]
fn verify_rejects_wrong_secret_same_id() {
    let (mut s, token) = store_with_one_key("laptop", Scope::View, None);
    let (id, _) = keys::parse_token(&token).unwrap();
    let wrong = format!("clk_{id}_{}", "x".repeat(43));
    assert_eq!(s.verify(&wrong, NOW).unwrap_err(), VerifyError::BadSecret);
    // 失敗不更新 last_used
    assert_eq!(s.list()[0].last_used_ms, None);
}

#[test]
fn verify_rejects_unknown_id_and_malformed() {
    let (mut s, _) = store_with_one_key("laptop", Scope::View, None);
    assert_eq!(
        s.verify(&format!("clk_zzzzzzzz_{}", "x".repeat(43)), NOW)
            .unwrap_err(),
        VerifyError::UnknownKey
    );
    assert_eq!(
        s.verify("not a token", NOW).unwrap_err(),
        VerifyError::Malformed
    );
}

#[test]
fn verify_rejects_expired_key_exactly_at_expiry() {
    let (mut s, token) = store_with_one_key("laptop", Scope::View, Some(NOW + 90 * DAY));
    assert!(s.verify(&token, NOW + 90 * DAY - 1).is_ok());
    assert_eq!(
        s.verify(&token, NOW + 90 * DAY).unwrap_err(),
        VerifyError::Expired
    );
}

#[test]
fn revoke_takes_effect_immediately_and_is_idempotent() {
    let (mut s, token) = store_with_one_key("laptop", Scope::View, None);
    let id = s.list()[0].id.clone();
    assert!(s.revoke(&id, NOW + 5), "第一次撤銷回 true");
    assert_eq!(s.verify(&token, NOW + 6).unwrap_err(), VerifyError::Revoked);
    assert!(!s.revoke(&id, NOW + 7), "已撤銷的再撤銷回 false");
    assert_eq!(s.list()[0].revoked_ms, Some(NOW + 5), "保留第一次的時間");
    assert!(!s.revoke("nope0000", NOW), "不存在的 id 回 false");
}

#[test]
fn constant_time_eq_semantics() {
    assert!(keys::ct_eq(b"abc", b"abc"));
    assert!(!keys::ct_eq(b"abc", b"abd"));
    assert!(!keys::ct_eq(b"abc", b"abcd"), "長度不同也是不相等");
    assert!(keys::ct_eq(b"", b""));
}

// ── keys.json ──────────────────────────────────────────────────────────────

#[test]
fn save_then_load_roundtrip_without_plaintext_secret() {
    let home = tmp("roundtrip");
    let (s, token) = store_with_one_key("laptop", Scope::View, Some(NOW + DAY));
    s.save(&home).unwrap();
    let text = std::fs::read_to_string(home.join("keys.json")).unwrap();
    let (_, secret) = keys::parse_token(&token).unwrap();
    assert!(!text.contains(secret), "明文 secret 不能落地");
    assert!(text.contains("\"version\""), "有版本號，之後才能升級");

    let mut loaded = KeyStore::load(&home).unwrap().expect("檔案存在");
    assert_eq!(loaded.list().len(), 1);
    assert_eq!(loaded.list()[0].expires_ms, Some(NOW + DAY));
    assert!(
        loaded.verify(&token, NOW).is_ok(),
        "重新載入後同一把金鑰仍可用"
    );
    assert_eq!(
        loaded.server_secret(),
        s.server_secret(),
        "簽 cookie 用的 server secret 要保留"
    );
}

#[test]
fn load_missing_file_is_none_not_error() {
    let home = tmp("missing");
    assert!(KeyStore::load(&home).unwrap().is_none());
}

#[test]
fn load_rejects_corrupt_json_with_path_in_message() {
    let home = tmp("corrupt");
    std::fs::write(home.join("keys.json"), "{ not json").unwrap();
    let err = KeyStore::load(&home).unwrap_err();
    assert!(err.contains("keys.json"), "{err}");
}

#[cfg(unix)]
#[test]
fn save_creates_file_with_mode_600() {
    use std::os::unix::fs::PermissionsExt;
    let home = tmp("mode");
    let (s, _) = store_with_one_key("laptop", Scope::View, None);
    s.save(&home).unwrap();
    let mode = std::fs::metadata(home.join("keys.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "{mode:o}");
    // 再存一次（檔案已存在）也維持 600
    s.save(&home).unwrap();
    let mode = std::fs::metadata(home.join("keys.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "{mode:o}");
}

#[cfg(unix)]
#[test]
fn load_rejects_group_or_world_readable_file_and_says_how_to_fix() {
    use std::os::unix::fs::PermissionsExt;
    let home = tmp("perm");
    let (s, _) = store_with_one_key("laptop", Scope::View, None);
    s.save(&home).unwrap();
    let p = home.join("keys.json");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = KeyStore::load(&home).unwrap_err();
    assert!(err.contains("600"), "要說怎麼修：{err}");
    assert!(err.contains("644"), "要說現在是什麼：{err}");
    // 修好就能讀
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(KeyStore::load(&home).unwrap().is_some());
}

// ── 到期字串 ───────────────────────────────────────────────────────────────

#[test]
fn parse_expires_accepts_days_hours_minutes_and_never() {
    assert_eq!(keys::parse_expires("90d", NOW), Ok(Some(NOW + 90 * DAY)));
    assert_eq!(
        keys::parse_expires("12h", NOW),
        Ok(Some(NOW + 12 * 60 * 60 * 1000))
    );
    assert_eq!(
        keys::parse_expires("30m", NOW),
        Ok(Some(NOW + 30 * 60 * 1000))
    );
    assert_eq!(keys::parse_expires("never", NOW), Ok(None));
    assert!(keys::parse_expires("90", NOW).is_err(), "沒有單位");
    assert!(keys::parse_expires("0d", NOW).is_err(), "0 沒有意義");
    assert!(keys::parse_expires("-1d", NOW).is_err());
    assert!(keys::parse_expires("1w", NOW).is_err(), "只收 d／h／m");
}

// ── CLI ────────────────────────────────────────────────────────────────────

mod cli {
    use super::*;
    use std::process::Command;

    fn coding_lab(home: &std::path::Path) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_coding-lab"));
        c.env("CODING_LAB_HOME", home);
        c
    }

    fn run(home: &std::path::Path, args: &[&str]) -> (bool, String, String) {
        let out = coding_lab(home).args(args).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    #[test]
    fn create_prints_token_once_list_hides_it_revoke_disables_it() {
        let home = tmp("cli");
        let (ok, out, err) = run(
            &home,
            &[
                "key",
                "create",
                "--name",
                "laptop",
                "--scope",
                "operate",
                "--expires",
                "90d",
            ],
        );
        assert!(ok, "{err}");
        let token = out
            .lines()
            .find_map(|l| l.split_whitespace().find(|w| w.starts_with("clk_")))
            .unwrap_or_else(|| panic!("stdout 要有金鑰：{out}"))
            .to_string();
        let (id, secret) = keys::parse_token(&token).unwrap();

        let (ok, out, _) = run(&home, &["key", "list"]);
        assert!(ok);
        assert!(out.contains(id), "list 要有 id：{out}");
        assert!(out.contains("laptop") && out.contains("operate"), "{out}");
        assert!(!out.contains(secret), "list 不能印 secret：{out}");

        // 用 library 驗證：CLI 寫的檔要能讓 collector 讀
        let mut s = KeyStore::load(&home).unwrap().unwrap();
        assert!(s.verify(&token, coding_lab::now_ms()).is_ok());

        let (ok, out, _) = run(&home, &["key", "revoke", id]);
        assert!(ok, "{out}");
        let mut s = KeyStore::load(&home).unwrap().unwrap();
        assert_eq!(
            s.verify(&token, coding_lab::now_ms()).unwrap_err(),
            VerifyError::Revoked
        );
        let (ok, out, _) = run(&home, &["key", "list"]);
        assert!(ok);
        assert!(out.contains("已撤銷") || out.contains("revoked"), "{out}");
    }

    #[test]
    fn create_requires_name_and_rejects_bad_scope_or_expires() {
        let home = tmp("cli-bad");
        assert!(!run(&home, &["key", "create"]).0, "沒有 --name");
        assert!(!run(&home, &["key", "create", "--name", "x", "--scope", "admin"]).0);
        assert!(!run(&home, &["key", "create", "--name", "x", "--expires", "1w"]).0);
        assert!(!home.join("keys.json").exists(), "失敗時不留檔");
    }

    #[test]
    fn revoke_unknown_id_fails_with_message() {
        let home = tmp("cli-unknown");
        let (ok, _, err) = run(&home, &["key", "revoke", "nope0000"]);
        assert!(!ok);
        assert!(err.contains("nope0000"), "{err}");
    }

    /// 同時啟動 `args_of(i)` 的 n 個程序，回每個的 (成功, stdout, stderr)。
    fn run_many(
        home: &std::path::Path,
        n: usize,
        args_of: impl Fn(usize) -> Vec<String>,
    ) -> Vec<(bool, String, String)> {
        let children: Vec<_> = (0..n)
            .map(|i| {
                coding_lab(home)
                    .args(args_of(i))
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        children
            .into_iter()
            .map(|c| {
                let out = c.wait_with_output().unwrap();
                (
                    out.status.success(),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                )
            })
            .collect()
    }

    /// W-L13（#782）：同一個 home 同時跑的 create 與 revoke 都不遺失、keys.json 不寫壞。
    /// 修正前：共用 keys.json.tmp、讀改寫沒有鎖，會少金鑰、偶爾整個檔寫壞（所有金鑰一起失效）。
    #[test]
    fn concurrent_creates_and_revokes_lose_nothing_and_keep_valid_json() {
        for round in 0..3 {
            let home = tmp(&format!("many-{round}"));
            for i in 0..5 {
                let (ok, _, err) = run(&home, &["key", "create", "--name", &format!("old{i}")]);
                assert!(ok, "{err}");
            }
            let old: Vec<String> = KeyStore::load(&home)
                .unwrap()
                .unwrap()
                .list()
                .iter()
                .map(|k| k.id.clone())
                .collect();
            let results = run_many(&home, 20, |i| {
                if i < 15 {
                    ["key", "create", "--name", &format!("new{i}")]
                        .map(String::from)
                        .to_vec()
                } else {
                    vec!["key".into(), "revoke".into(), old[i - 15].clone()]
                }
            });
            for (i, (ok, _, err)) in results.iter().enumerate() {
                assert!(ok, "第 {round} 輪第 {i} 個失敗：{err}");
            }
            let text = std::fs::read_to_string(KeyStore::path(&home)).unwrap();
            let store = KeyStore::load(&home)
                .unwrap_or_else(|e| panic!("第 {round} 輪 keys.json 壞了：{e}\n{text}"))
                .unwrap();
            let names: std::collections::BTreeSet<&str> =
                store.list().iter().map(|k| k.name.as_str()).collect();
            assert_eq!(store.list().len(), 20, "第 {round} 輪：{names:?}");
            for i in 0..15 {
                assert!(
                    names.contains(format!("new{i}").as_str()),
                    "第 {round} 輪少了 new{i}"
                );
            }
            for id in &old {
                assert!(
                    store.get(id).unwrap().revoked_ms.is_some(),
                    "第 {round} 輪：{id} 的撤銷被蓋掉了"
                );
            }
        }
    }

    /// W-L13：寫入前留一份 keys.json.bak；keys.json 寫壞時，錯誤訊息指向它。
    #[test]
    fn writes_keep_a_backup_and_a_corrupt_file_points_to_it() {
        let home = tmp("bak");
        assert!(run(&home, &["key", "create", "--name", "a"]).0);
        assert!(run(&home, &["key", "create", "--name", "b"]).0);
        let bak = home.join("keys.json.bak");
        let text = std::fs::read_to_string(&bak).expect("要留 keys.json.bak");
        assert!(
            text.contains("\"a\"") && !text.contains("\"b\""),
            "是寫入前的內容：{text}"
        );
        std::fs::write(KeyStore::path(&home), "{ 壞掉").unwrap();
        let (ok, _, err) = run(&home, &["key", "list"]);
        assert!(!ok);
        assert!(err.contains("keys.json.bak"), "要說可以從 .bak 還原：{err}");
        // 壞掉的檔不會蓋掉好的 .bak
        let (ok, _, _) = run(&home, &["key", "create", "--name", "c"]);
        assert!(!ok, "keys.json 壞掉時不寫");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), text);
    }

    /// W-L13：login-links.json 一樣，同時建的一次性連結都在、都能用。
    #[test]
    fn concurrent_login_links_are_all_kept() {
        let home = tmp("links");
        assert!(run(&home, &["key", "create", "--name", "a"]).0);
        let id = KeyStore::load(&home).unwrap().unwrap().list()[0].id.clone();
        let results = run_many(&home, 20, |_| {
            ["key", "login-link", "--key", &id]
                .map(String::from)
                .to_vec()
        });
        let codes: Vec<String> = results
            .iter()
            .map(|(ok, out, err)| {
                assert!(ok, "{err}");
                out.split("/auth/once/")
                    .nth(1)
                    .and_then(|r| r.split_whitespace().next())
                    .unwrap_or_else(|| panic!("沒有連結：{out}"))
                    .to_string()
            })
            .collect();
        let now = coding_lab::now_ms();
        for code in &codes {
            assert_eq!(
                coding_lab::auth::consume_once_link(&home, code, now).unwrap(),
                Some(id.clone()),
                "連結 {code} 不見了"
            );
        }
    }
}

/// W-L1：單位不是 ASCII 時回錯誤（原本位元組切片切在「天」的中間而 panic）；乘出來溢位也回錯誤。
#[test]
fn parse_expires_rejects_non_ascii_units_and_overflow() {
    for bad in [
        "9天",
        "天",
        "9d天",
        "18446744073709551615d",
        "99999999999999999m",
    ] {
        assert!(keys::parse_expires(bad, NOW).is_err(), "{bad}");
    }
    assert!(
        keys::parse_expires("1d", u64::MAX - 1).is_err(),
        "加上 now 溢位"
    );
}
