//! `coding-lab service install／uninstall／status`（#662，PLAN「網路綁定」形態 B）：
//! 三平台的 unit 檔內容、安裝步驟、uninstall 要清乾淨。檔案與指令都先算成 Plan（純函式），再由 apply 執行。

use std::path::{Path, PathBuf};

use coding_lab::service::{self, Ctx, Plan, Platform, Step};

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
        .find(|(p, _)| p.to_string_lossy().ends_with(suffix))
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
    assert!(
        xml.contains(r"<Command>C:\Users\u\AppData\Local\coding-lab\coding-lab.exe</Command>"),
        "{xml}"
    );
    assert!(
        xml.contains(
            r#"<Arguments>serve --home "C:\Users\u\.coding-lab" --idle-exit-seconds 0</Arguments>"#
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
        cmds.iter()
            .any(|c| c.starts_with("schtasks /Create /TN CodingLab /XML ") && c.ends_with(" /F")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c == "schtasks /Run /TN CodingLab"),
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
        .any(|c| c == "schtasks /Delete /TN CodingLab /F")
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
    let mut ran: Vec<Step> = Vec::new();
    service::apply(&install, &mut |cmd| {
        ran.push(Step::Command(cmd.to_vec()));
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
    assert_eq!(cmds(Platform::Windows), "schtasks /Query /TN CodingLab");
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
    let err = service::install(
        &plan,
        &mut |cmd| Err(format!("{} 被拒", cmd[0])),
        &mut || restored += 1,
    )
    .unwrap_err();
    assert!(err.contains("systemctl") && err.contains("被拒"), "{err}");
    assert_eq!(restored, 1, "註冊失敗要把先前停掉的 collector 拉回來");
    assert!(
        !dir.join("config/systemd/user/coding-lab.service").exists(),
        "失敗時不留下半套：unit 檔要收回"
    );
    let mut restored = 0;
    service::install(&plan, &mut |_| Ok(()), &mut || restored += 1).unwrap();
    assert_eq!(restored, 0, "成功就不用拉回");
}

#[test]
fn non_utf8_command_output_is_flagged_instead_of_shown_as_garbage() {
    let (text, lossy) = service::decode_output(b"ERROR: Access is denied.\r\n");
    assert_eq!((text.as_str(), lossy), ("ERROR: Access is denied.", false));
    // cp950 的「錯誤: 存取被拒。」不是合法 UTF-8
    let (text, lossy) = service::decode_output(b"\xbf\xf9\xbb~: \xa6s\xa8\xfa\xb3Q\xa9\xda\xa1C");
    assert!(lossy);
    assert!(!text.is_empty());
}

/// #724：Windows 的 schtasks 用 OEM code page 輸出，要解成可讀文字（實機拿到的 cp950 位元組）
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
