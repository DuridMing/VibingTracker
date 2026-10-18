//! `coding-lab service install／uninstall／status`（#662，PLAN「網路綁定」形態 B）：
//! 三平台的 unit 檔內容、安裝步驟、uninstall 要清乾淨。檔案與指令都先算成 Plan（純函式），再由 apply 執行。

use std::path::{Path, PathBuf};

use coding_lab::service::{self, Ctx, Plan, Platform};

fn ctx(home: &Path) -> Ctx {
    Ctx {
        exe: PathBuf::from("/opt/coding-lab/bin/coding-lab"),
        home: home.to_path_buf(),
        config_home: PathBuf::from("/home/u/.config"),
        user_home: PathBuf::from("/home/u"),
        uid: Some(1000),
        user: None,
    }
}

fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("service-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn file_of<'a>(plan: &'a Plan, suffix: &str) -> &'a str {
    plan.files
        .iter()
        // Windows 上 Path::join 產生 `\`，比對前統一成 `/`
        .find(|(p, _)| p.to_string_lossy().replace('\\', "/").ends_with(suffix))
        .map(|(_, c)| c.as_str())
        .unwrap_or_else(|| panic!("plan 裡沒有 {suffix}：{:?}", plan.files))
}

#[test]
fn linux_writes_a_systemd_user_unit_that_keeps_the_collector_resident() {
    let plan = service::plan_install(Platform::Linux, &ctx(Path::new("/home/u/.coding-lab")));
    let unit = file_of(&plan, ".config/systemd/user/coding-lab.service");
    assert!(unit.contains("ExecStart=/opt/coding-lab/bin/coding-lab serve --home /home/u/.coding-lab --idle-exit-seconds 0"), "{unit}");
    assert!(unit.contains("Restart=on-failure"), "{unit}");
    assert!(
        unit.contains("Environment=CODING_LAB_HOME=/home/u/.coding-lab"),
        "ExecStop 的 stop 沒有 --home，要靠環境變數：{unit}"
    );
    assert!(unit.contains("WantedBy=default.target"), "{unit}");
    // R-7（#739）：開機時 bind 失敗（網路還沒好、埠被佔）要一直重試，不撞 systemd 預設的 5 次／10 秒上限
    let unit_section = unit.split("[Service]").next().unwrap();
    assert!(
        unit_section.contains("StartLimitIntervalSec=0"),
        "要在 [Unit]：{unit}"
    );
    let cmds: Vec<String> = plan.commands.iter().map(|c| c.join(" ")).collect();
    assert!(
        cmds.iter().any(|c| c == "systemctl --user daemon-reload"),
        "{cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c == "systemctl --user enable --now coding-lab.service"),
        "{cmds:?}"
    );
    // 開機（還沒登入）就要起來：要 linger，裝完提醒
    assert!(
        plan.notes
            .iter()
            .any(|n| n.contains("loginctl enable-linger")),
        "{:?}",
        plan.notes
    );
}

#[test]
fn macos_writes_a_launch_agent_and_bootstraps_it_for_the_users_gui_domain() {
    let plan = service::plan_install(Platform::Macos, &ctx(Path::new("/Users/u/.coding-lab")));
    let plist = file_of(&plan, "Library/LaunchAgents/io.coding-lab.collector.plist");
    assert!(
        plist.contains("<string>io.coding-lab.collector</string>"),
        "{plist}"
    );
    assert!(
        plist.contains("<string>/opt/coding-lab/bin/coding-lab</string>"),
        "{plist}"
    );
    assert!(plist.contains("<string>serve</string>"), "{plist}");
    assert!(
        plist.contains("<string>--idle-exit-seconds</string>")
            && plist.contains("<string>0</string>"),
        "{plist}"
    );
    assert!(plist.contains("<key>KeepAlive</key>"), "{plist}");
    let cmds: Vec<String> = plan.commands.iter().map(|c| c.join(" ")).collect();
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("launchctl bootstrap gui/1000 ")),
        "{cmds:?}"
    );
}

/// R-15（#739）：systemd 會展開 `%` 指定符，Exec 行還會展開 `$` 變數：路徑裡的要寫成 `%%`、`$$`
/// （Environment= 只展開 `%`）。有空白、引號、反斜線的路徑加引號、裡面的 `\` 與 `"` 跳脫。
#[test]
fn systemd_unit_escapes_specifiers_variables_and_quotes() {
    let mut c = ctx(Path::new("/home/u/100%/$x/.coding-lab"));
    c.exe = PathBuf::from("/opt/my apps/coding-lab");
    let plan = service::plan_install(Platform::Linux, &c);
    let unit = file_of(&plan, "coding-lab.service");
    assert!(
        unit.contains(
            "ExecStart=\"/opt/my apps/coding-lab\" serve --home /home/u/100%%/$$x/.coding-lab --idle-exit-seconds 0\n"
        ),
        "{unit}"
    );
    assert!(
        unit.contains("ExecStop=\"/opt/my apps/coding-lab\" stop\n"),
        "{unit}"
    );
    assert!(
        unit.contains("Environment=CODING_LAB_HOME=/home/u/100%%/$x/.coding-lab\n"),
        "{unit}"
    );
    let mut c = ctx(Path::new(r#"/home/u/a"b\c"#));
    c.exe = PathBuf::from("/opt/coding-lab/bin/coding-lab");
    let unit = file_of(
        &service::plan_install(Platform::Linux, &c),
        "coding-lab.service",
    )
    .to_string();
    assert!(unit.contains(r#"--home "/home/u/a\"b\\c" "#), "{unit}");
}

/// W-M6（#739）：`<Arguments>` 照 Windows 的命令列規則加引號；結尾的 `\` 在引號前要加倍，
/// 否則 `"C:\x\"` 的引號被跳脫、後面的參數全黏進 home。
#[test]
fn windows_task_arguments_survive_a_trailing_backslash() {
    let c = Ctx {
        exe: PathBuf::from(r"C:\Program Files\coding-lab\coding-lab.exe"),
        home: PathBuf::from(r"D:\data\"),
        config_home: PathBuf::from(r"C:\Users\u\AppData\Roaming"),
        user_home: PathBuf::from(r"C:\Users\u"),
        uid: None,
        user: None,
    };
    let xml = file_of(
        &service::plan_install(Platform::Windows, &c),
        "coding-lab-task.xml",
    )
    .to_string();
    assert!(
        xml.contains(
            r#"<Arguments>--headless &quot;C:\Program Files\coding-lab\coding-lab.exe&quot; serve --home &quot;D:\data\\&quot; --idle-exit-seconds 0</Arguments>"#
        ),
        "{xml}"
    );
}

/// R-6（#739）：`coding-lab stop` 停掉的是常駐服務本身，三個平台都不會自己再起來（正常結束不算失敗）；
/// stop 之後提示用服務管理員的指令把它叫回來。
#[test]
fn start_command_brings_the_resident_collector_back() {
    let c = ctx(Path::new("/home/u/.coding-lab"));
    let j = |p| service::start_command(p, &c).join(" ");
    assert_eq!(
        j(Platform::Linux),
        "systemctl --user start coding-lab.service"
    );
    assert_eq!(
        j(Platform::Macos),
        "launchctl kickstart gui/1000/io.coding-lab.collector"
    );
    assert_eq!(
        j(Platform::Windows),
        r"schtasks /Run /TN \CodingLab\default"
    );
}

#[test]
fn windows_writes_a_scheduled_task_xml_that_runs_at_logon_and_registers_it() {
    let c = Ctx {
        exe: PathBuf::from(r"C:\Users\u\AppData\Local\coding-lab\coding-lab.exe"),
        home: PathBuf::from(r"C:\Users\u\.coding-lab"),
        config_home: PathBuf::from(r"C:\Users\u\AppData\Roaming"),
        user_home: PathBuf::from(r"C:\Users\u"),
        uid: None,
        user: Some(r"DESKTOP-1\u".into()),
    };
    let plan = service::plan_install(Platform::Windows, &c);
    let xml = file_of(&plan, "coding-lab-task.xml");
    assert!(xml.contains("<LogonTrigger>"), "{xml}");
    // L-12（#739）：console 程式直接當 Exec 會在登入時開出主控台視窗、關掉就殺掉 collector；
    // 經 `conhost.exe --headless` 跑就沒有視窗（`<Hidden>` 只是在工作排程器的清單裡隱藏）
    assert!(
        xml.contains(r"<Command>%SystemRoot%\System32\conhost.exe</Command>"),
        "{xml}"
    );
    assert!(
        xml.contains(
            r#"<Arguments>--headless &quot;C:\Users\u\AppData\Local\coding-lab\coding-lab.exe&quot; serve --home &quot;C:\Users\u\.coding-lab&quot; --idle-exit-seconds 0</Arguments>"#
        ),
        "{xml}"
    );
    assert!(xml.contains("<Hidden>true</Hidden>"), "{xml}");
    // #719：沒有 UserId 等於「任何使用者登入時」，一般使用者建不了（Access is denied）：trigger 與 principal 都要有目前使用者
    assert_eq!(
        xml.matches(r"<UserId>DESKTOP-1\u</UserId>").count(),
        2,
        "{xml}"
    );
    assert!(
        xml.contains(
            "<LogonTrigger>\r\n      <Enabled>true</Enabled>\r\n      <UserId>DESKTOP-1\\u</UserId>"
        ),
        "{xml}"
    );
    let cmds: Vec<String> = plan.commands.iter().map(|c| c.join(" ")).collect();
    assert!(
        cmds.iter().any(
            |c| c.starts_with(r"schtasks /Create /TN \CodingLab\u /XML ") && c.ends_with(" /F")
        ),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c == r"schtasks /Run /TN \CodingLab\u"),
        "{cmds:?}"
    );
}

#[test]
fn uninstall_removes_exactly_what_install_wrote_on_every_platform() {
    for (platform, c) in [
        (Platform::Linux, ctx(Path::new("/home/u/.coding-lab"))),
        (Platform::Macos, ctx(Path::new("/Users/u/.coding-lab"))),
        (Platform::Windows, ctx(Path::new(r"C:\Users\u\.coding-lab"))),
    ] {
        let install = service::plan_install(platform, &c);
        let uninstall = service::plan_uninstall(platform, &c);
        let written: Vec<&PathBuf> = install.files.iter().map(|(p, _)| p).collect();
        assert_eq!(
            uninstall.removes,
            written.into_iter().cloned().collect::<Vec<_>>(),
            "{platform:?}"
        );
        assert!(
            uninstall.files.is_empty(),
            "{platform:?} uninstall 不該寫檔"
        );
        assert!(
            !uninstall.commands.is_empty(),
            "{platform:?} 要有停掉／取消註冊的指令"
        );
    }
    let cmds = |p: Platform, c: &Ctx| -> Vec<String> {
        service::plan_uninstall(p, c)
            .commands
            .iter()
            .map(|x| x.join(" "))
            .collect()
    };
    assert!(
        cmds(Platform::Linux, &ctx(Path::new("/home/u/.coding-lab")))
            .iter()
            .any(|c| c == "systemctl --user disable --now coding-lab.service")
    );
    assert!(
        cmds(Platform::Macos, &ctx(Path::new("/Users/u/.coding-lab")))
            .iter()
            .any(|c| c == "launchctl bootout gui/1000/io.coding-lab.collector")
    );
    assert!(
        cmds(
            Platform::Windows,
            &ctx(Path::new(r"C:\Users\u\.coding-lab"))
        )
        .iter()
        .any(|c| c == r"schtasks /Delete /TN \CodingLab\default /F")
    );
}

#[test]
fn apply_writes_files_then_runs_commands_and_uninstall_leaves_nothing_behind() {
    let dir = tmp("apply");
    let c = Ctx {
        exe: PathBuf::from("/opt/coding-lab/bin/coding-lab"),
        home: dir.join("home"),
        config_home: dir.join("config"),
        user_home: dir.join("user"),
        uid: Some(1000),
        user: None,
    };
    let install = service::plan_install(Platform::Linux, &c);
    let mut ran: Vec<Vec<String>> = Vec::new();
    service::apply(&install, &mut |cmd| {
        ran.push(cmd.to_vec());
        Ok(())
    })
    .unwrap();
    let unit = dir.join("config/systemd/user/coding-lab.service");
    assert!(unit.is_file(), "unit 檔要寫到 config_home 底下");
    assert!(ran.len() >= 2, "要跑 daemon-reload 與 enable：{ran:?}");
    // 指令失敗要回錯誤、說是哪一條
    let err =
        service::apply(&install, &mut |cmd| Err(format!("假裝 {} 失敗", cmd[0]))).unwrap_err();
    assert!(err.contains("systemctl"), "{err}");

    let uninstall = service::plan_uninstall(Platform::Linux, &c);
    service::apply(&uninstall, &mut |_| Ok(())).unwrap();
    assert!(!unit.exists(), "uninstall 要刪掉 unit 檔");
    // 再 uninstall 一次：檔案已經不在也不算錯
    service::apply(&uninstall, &mut |_| Ok(())).unwrap();
}

#[test]
fn status_reports_whether_the_unit_file_is_installed() {
    let dir = tmp("status");
    let c = Ctx {
        exe: PathBuf::from("/opt/coding-lab/bin/coding-lab"),
        home: dir.join("home"),
        config_home: dir.join("config"),
        user_home: dir.join("user"),
        uid: Some(1000),
        user: None,
    };
    assert!(!service::installed(Platform::Linux, &c));
    service::apply(&service::plan_install(Platform::Linux, &c), &mut |_| Ok(())).unwrap();
    assert!(service::installed(Platform::Linux, &c));
}

#[test]
fn current_platform_is_known_on_the_three_supported_oses() {
    let p = Platform::current();
    assert!(matches!(
        p,
        Some(Platform::Linux | Platform::Macos | Platform::Windows)
    ));
}

// ---------- #719：status 以系統查詢為準、install 失敗把 collector 拉回、非 UTF-8 輸出 ----------

#[test]
fn status_asks_the_service_manager_not_just_the_file() {
    let c = ctx(Path::new("/home/u/.coding-lab"));
    let cmds = |p: Platform| service::status_command(p, &c).join(" ");
    assert_eq!(
        cmds(Platform::Linux),
        "systemctl --user is-enabled coding-lab.service"
    );
    assert_eq!(
        cmds(Platform::Macos),
        "launchctl print gui/1000/io.coding-lab.collector"
    );
    assert_eq!(
        cmds(Platform::Windows),
        r"schtasks /Query /TN \CodingLab\default"
    );
    for p in [Platform::Linux, Platform::Macos, Platform::Windows] {
        assert!(
            service::query_installed(p, &c, &mut |_| Ok("enabled\n".into())),
            "{p:?} 查得到就是已安裝"
        );
        assert!(
            !service::query_installed(p, &c, &mut |_| Err("not found".into())),
            "{p:?} 查不到就是未安裝，不管 XML／unit 檔在不在"
        );
    }
    assert!(
        !service::query_installed(Platform::Linux, &c, &mut |_| Ok("disabled\n".into())),
        "Linux 的 unit 在但沒 enable 不算已安裝"
    );
}

#[test]
fn install_restores_the_collector_when_registration_fails() {
    let dir = tmp("restore");
    let c = Ctx {
        exe: PathBuf::from("/opt/coding-lab/bin/coding-lab"),
        home: dir.join("home"),
        config_home: dir.join("config"),
        user_home: dir.join("user"),
        uid: Some(1000),
        user: None,
    };
    let plan = service::plan_install(Platform::Linux, &c);
    let mut restored = 0;
    let mut logged: Vec<String> = Vec::new();
    let err = service::install(
        &plan,
        &mut |cmd| Err(format!("{} 被拒", cmd[0])),
        &mut || restored += 1,
        &mut |m| logged.push(m.to_string()),
    )
    .unwrap_err();
    assert!(err.contains("systemctl") && err.contains("被拒"), "{err}");
    assert_eq!(restored, 1, "註冊失敗要把先前停掉的 collector 拉回來");
    // #726：失敗要寫進 collector.log（指令與可讀錯誤），不只印在 stderr
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(
        logged[0].contains("service")
            && logged[0].contains("systemctl")
            && logged[0].contains("被拒"),
        "{logged:?}"
    );
    assert!(
        !dir.join("config/systemd/user/coding-lab.service").exists(),
        "失敗時不留下半套：unit 檔要收回"
    );
    let mut restored = 0;
    let mut logged: Vec<String> = Vec::new();
    service::install(&plan, &mut |_| Ok(()), &mut || restored += 1, &mut |m| {
        logged.push(m.to_string())
    })
    .unwrap();
    assert_eq!(restored, 0, "成功就不用拉回");
    assert!(logged.is_empty(), "成功不寫失敗紀錄：{logged:?}");
}

#[test]
fn non_utf8_command_output_is_flagged_instead_of_shown_as_garbage() {
    let (text, lossy) = service::decode_output(b"ERROR: Access is denied.\r\n");
    assert_eq!((text.as_str(), lossy), ("ERROR: Access is denied.", false));
    // cp950 的「錯誤: 存取被拒。」不是合法 UTF-8；沒有可用的 code page 時只能 lossy。
    // code page 用注入的：在 OEM code page 950 的 Windows 上 decode_output 會解碼成功（#724），斷言就不成立（#732）
    let (text, lossy) =
        service::decode_output_with(b"\xbf\xf9\xbb~: \xa6s\xa8\xfa\xb3Q\xa9\xda\xa1C", None);
    assert!(lossy);
    assert!(!text.is_empty());
}

/// #724：Windows 的 schtasks 用 OEM code page 輸出，要解成可讀文字（實機拿到的 cp950 位元組）。
/// encoding_rs 只在 Windows 編（#758），這個測試由 Windows 的 check.sh／CI 跑
#[cfg(windows)]
#[test]
fn oem_code_page_bytes_decode_to_readable_text() {
    // 「錯誤: 存取被拒。\r\n」的 cp950 位元組（#715 Windows 側實機紀錄）
    let denied = [
        0xbf, 0xf9, 0xbb, 0x7e, 0x3a, 0x20, 0xa6, 0x73, 0xa8, 0xfa, 0xb3, 0x51, 0xa9, 0xda, 0xa1,
        0x43, 0x0d, 0x0a,
    ];
    assert_eq!(
        service::decode_with_code_page(&denied, 950).as_deref(),
        Some("錯誤: 存取被拒。")
    );
    // 「錯誤: 系統找不到指定的檔案。」
    let not_found = [
        0xbf, 0xf9, 0xbb, 0x7e, 0x3a, 0x20, 0xa8, 0x74, 0xb2, 0xce, 0xa7, 0xe4, 0xa4, 0xa3, 0xa8,
        0xec, 0xab, 0xfc, 0xa9, 0x77, 0xaa, 0xba, 0xc0, 0xc9, 0xae, 0xd7, 0xa1, 0x43, 0x0d, 0x0a,
    ];
    assert_eq!(
        service::decode_with_code_page(&not_found, 950).as_deref(),
        Some("錯誤: 系統找不到指定的檔案。")
    );
    assert_eq!(service::code_page_label(936), Some("gbk"));
    assert_eq!(service::code_page_label(1252), Some("windows-1252"));
    assert_eq!(service::code_page_label(12345), None);
    assert!(service::decode_with_code_page(&denied, 12345).is_none());
    // UTF-8 照舊直接用
    assert_eq!(
        service::decode_output("ok\n".as_bytes()),
        ("ok".to_string(), false)
    );
}

/// 非 Windows 不帶 encoding_rs：code page 對照表照常，解碼一律交給 UTF-8／lossy（#758 E-B9）
#[cfg(not(windows))]
#[test]
fn code_page_decoding_is_windows_only() {
    assert_eq!(service::code_page_label(950), Some("big5"));
    assert_eq!(service::code_page_label(12345), None);
    assert!(service::decode_with_code_page(&[0xbf, 0xf9], 950).is_none());
    assert_eq!(
        service::decode_output("ok\n".as_bytes()),
        ("ok".to_string(), false)
    );
}

/// #724：查詢「沒裝」是預期結果，quiet 版不寫 log 也不印十六進位，但錯誤訊息照樣帶回
#[cfg(unix)]
#[test]
fn quiet_capture_returns_the_error_text_without_decorating_it() {
    let cmd: Vec<String> = ["sh", "-c", "echo not-installed >&2; exit 1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let err = service::run_command_capture_quiet(&cmd).unwrap_err();
    assert_eq!(err, "not-installed");
    let ok: Vec<String> = ["sh", "-c", "echo enabled"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(service::run_command_capture_quiet(&ok).unwrap(), "enabled");
    // query_installed 用 quiet 版：查不到就是 false，不會 panic
    let c = ctx(Path::new("/home/u/.coding-lab"));
    assert!(!service::query_installed(
        Platform::Linux,
        &c,
        &mut |_| Err("not found".into())
    ));
}

/// W-L11（#781）：同一個帳號用兩個 home 各 install 一次，第二次被拒並說出原本服務的 home；`--force` 才換。
#[test]
fn a_second_install_with_another_home_is_refused() {
    let dir = tmp("two-homes");
    let mk = |home: &str| Ctx {
        exe: PathBuf::from("/opt/coding-lab/bin/coding-lab"),
        home: dir.join(home),
        config_home: dir.join("config"),
        user_home: dir.join("user"),
        uid: Some(1000),
        user: None,
    };
    let (a, b) = (mk("home a"), mk("home-b"));
    let mut ok = |_: &[String]| -> Result<String, String> { Ok(String::new()) };
    assert_eq!(service::existing_home(Platform::Linux, &a, &mut ok), None);
    service::apply(&service::plan_install(Platform::Linux, &a), &mut |_| Ok(())).unwrap();
    let existing = service::existing_home(Platform::Linux, &b, &mut ok);
    assert_eq!(existing.as_deref(), Some(a.home.as_path()));
    let err = service::check_existing(existing.as_deref(), &b.home, false).unwrap_err();
    assert!(
        err.contains(&a.home.display().to_string()) && err.contains("--force"),
        "{err}"
    );
    assert!(service::check_existing(existing.as_deref(), &b.home, true).is_ok());
    assert!(
        service::check_existing(existing.as_deref(), &a.home, false).is_ok(),
        "同一個 home 重裝照常"
    );
}

/// W-L11：三個平台寫出去的設定都讀得回原本的 home（含空白、引號、%、$、結尾的反斜線）。
#[test]
fn the_home_is_read_back_from_what_install_wrote() {
    for home in [
        "/home/u/.coding-lab",
        "/home/u/a b/100%/$x",
        r#"/home/u/q"uote"#,
    ] {
        let c = ctx(Path::new(home));
        let unit = file_of(
            &service::plan_install(Platform::Linux, &c),
            "coding-lab.service",
        )
        .to_string();
        assert_eq!(
            service::home_from_unit(&unit).as_deref(),
            Some(Path::new(home)),
            "{unit}"
        );
        let plist = file_of(&service::plan_install(Platform::Macos, &c), ".plist").to_string();
        assert_eq!(
            service::home_from_plist(&plist).as_deref(),
            Some(Path::new(home)),
            "{plist}"
        );
    }
    for home in [
        r"C:\Users\u\.coding-lab",
        r"D:\data\",
        r#"C:\a "b"\c"#,
        r"C:\x y\z",
    ] {
        let c = Ctx {
            exe: PathBuf::from(r"C:\Program Files\coding-lab\coding-lab.exe"),
            home: PathBuf::from(home),
            config_home: PathBuf::from(r"C:\Users\u\AppData\Roaming"),
            user_home: PathBuf::from(r"C:\Users\u"),
            uid: None,
            user: Some(r"DESKTOP-1\u".into()),
        };
        let xml = file_of(
            &service::plan_install(Platform::Windows, &c),
            "coding-lab-task.xml",
        )
        .to_string();
        assert_eq!(
            service::home_from_task_xml(&xml).as_deref(),
            Some(Path::new(home)),
            "{xml}"
        );
    }
}

/// W-L11：Windows 的工作放在 `\CodingLab\<使用者>`：根目錄的名稱整台機器共用，別的帳號會裝不起來或被蓋掉。
#[test]
fn windows_task_is_named_after_the_user() {
    let mut c = ctx(Path::new(r"C:\Users\u\.coding-lab"));
    c.user = Some(r"DESKTOP-1\Alice".into());
    assert_eq!(service::windows_task(&c), r"\CodingLab\Alice");
    let cmds: Vec<String> = service::plan_install(Platform::Windows, &c)
        .commands
        .iter()
        .map(|c| c.join(" "))
        .collect();
    assert!(
        cmds.iter()
            .any(|c| c.starts_with(r"schtasks /Create /TN \CodingLab\Alice /XML ")),
        "{cmds:?}"
    );
    let un: Vec<String> = service::plan_uninstall(Platform::Windows, &c)
        .commands
        .iter()
        .map(|c| c.join(" "))
        .collect();
    assert!(
        un.iter()
            .any(|c| c == r"schtasks /Delete /TN \CodingLab\Alice /F"),
        "{un:?}"
    );
    c.user = Some("we/ird:name".into());
    assert_eq!(service::windows_task(&c), r"\CodingLab\we_ird_name");
}
