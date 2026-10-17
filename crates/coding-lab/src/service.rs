//! `coding-lab service install／uninstall／status`（#662，PLAN「網路綁定」形態 B）。
//!
//! 伺服器重開機後 collector 要自己起來，不等第一個 Claude Code session 的 `ensure`：
//! Linux 寫 systemd **user** unit、macOS 寫 LaunchAgent、Windows 用工作排程器（登入時啟動、隱藏視窗）。
//! 三平台都是「使用者層級」的常駐，不需要 root；都用 `serve --home <home> --idle-exit-seconds 0`，
//! 所以 collector 不會像平常一樣閒置 10 分鐘就退出。
//!
//! 要寫哪些檔、跑哪些指令先算成 [`Plan`]（純函式，可測），再由 [`apply`] 執行；`uninstall` 只刪 install 寫的檔。

use std::path::{Path, PathBuf};

pub const LINUX_UNIT: &str = "coding-lab.service";
pub const MACOS_LABEL: &str = "io.coding-lab.collector";
pub const WINDOWS_TASK: &str = "CodingLab";
pub const WINDOWS_XML: &str = "coding-lab-task.xml";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Macos,
    Windows,
}

impl Platform {
    /// 這台是哪一種；其他 OS 回 None（不支援 service）
    pub fn current() -> Option<Platform> {
        match std::env::consts::OS {
            "linux" => Some(Platform::Linux),
            "macos" => Some(Platform::Macos),
            "windows" => Some(Platform::Windows),
            _ => None,
        }
    }
}

/// 算 Plan 需要知道的環境：二進位在哪、資料目錄、使用者的設定目錄與家目錄、uid（macOS 的 launchctl gui/<uid>）
#[derive(Clone, Debug)]
pub struct Ctx {
    pub exe: PathBuf,
    pub home: PathBuf,
    /// Linux：`$XDG_CONFIG_HOME`（預設 ~/.config）；macOS、Windows 不用
    pub config_home: PathBuf,
    pub user_home: PathBuf,
    pub uid: Option<u32>,
}

impl Ctx {
    /// 從真正的環境組出來；`home` 用設定算出的資料目錄
    pub fn detect(home: &Path) -> Result<Ctx, String> {
        let exe = std::env::current_exe().map_err(|e| format!("找不到自己的路徑：{e}"))?;
        let user_home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or("沒有 HOME／USERPROFILE，不知道家目錄在哪")?;
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| user_home.join(".config"));
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok());
        Ok(Ctx {
            exe,
            home: home.to_path_buf(),
            config_home,
            user_home,
            uid,
        })
    }
}

/// apply 的一步（測試用來記錄跑了什麼）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Command(Vec<String>),
}

/// 要做的事：先寫 `files`、刪 `removes`，再依序跑 `commands`；`notes` 是做完要提醒使用者的話
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub files: Vec<(PathBuf, String)>,
    pub removes: Vec<PathBuf>,
    pub commands: Vec<Vec<String>>,
    pub notes: Vec<String>,
}

fn unit_path(ctx: &Ctx) -> PathBuf {
    ctx.config_home.join("systemd/user").join(LINUX_UNIT)
}

fn plist_path(ctx: &Ctx) -> PathBuf {
    ctx.user_home
        .join("Library/LaunchAgents")
        .join(format!("{MACOS_LABEL}.plist"))
}

fn task_xml_path(ctx: &Ctx) -> PathBuf {
    ctx.home.join(WINDOWS_XML)
}

/// install 會寫的檔（status 與 uninstall 都看這個）
pub fn files_for(platform: Platform, ctx: &Ctx) -> Vec<PathBuf> {
    match platform {
        Platform::Linux => vec![unit_path(ctx)],
        Platform::Macos => vec![plist_path(ctx)],
        Platform::Windows => vec![task_xml_path(ctx)],
    }
}

pub fn installed(platform: Platform, ctx: &Ctx) -> bool {
    files_for(platform, ctx).iter().all(|p| p.is_file())
}

fn s(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

fn xml_escape(t: &str) -> String {
    t.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// systemd 的 ExecStart 用空白分隔參數：路徑有空白要加引號
fn systemd_quote(p: &Path) -> String {
    let t = p.to_string_lossy();
    if t.contains(' ') {
        format!("\"{}\"", t.replace('"', "\\\""))
    } else {
        t.into_owned()
    }
}

pub fn plan_install(platform: Platform, ctx: &Ctx) -> Plan {
    let home = ctx.home.to_string_lossy().into_owned();
    match platform {
        Platform::Linux => {
            let unit = format!(
                "# coding-lab collector（由 `coding-lab service install` 產生）\n\
                 [Unit]\n\
                 Description=Coding Lab collector（Claude Code session 追蹤）\n\
                 After=network.target\n\
                 \n\
                 [Service]\n\
                 Type=simple\n\
                 Environment=CODING_LAB_HOME={home}\n\
                 ExecStart={exe} serve --home {home} --idle-exit-seconds 0\n\
                 ExecStop={exe} stop\n\
                 Restart=on-failure\n\
                 RestartSec=2\n\
                 \n\
                 [Install]\n\
                 WantedBy=default.target\n",
                exe = systemd_quote(&ctx.exe),
                home = systemd_quote(&ctx.home),
            );
            Plan {
                files: vec![(unit_path(ctx), unit)],
                removes: vec![],
                commands: vec![
                    s(&["systemctl", "--user", "daemon-reload"]),
                    s(&["systemctl", "--user", "enable", "--now", LINUX_UNIT]),
                ],
                notes: vec![
                    "開機（還沒有人登入）就要起來的話，執行一次：loginctl enable-linger $USER"
                        .into(),
                    format!(
                        "查看：systemctl --user status {LINUX_UNIT}；紀錄：journalctl --user -u {LINUX_UNIT} 與 {home}/collector.log"
                    ),
                ],
            }
        }
        Platform::Macos => {
            let plist = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                 <plist version=\"1.0\">\n\
                 <dict>\n\
                 \t<key>Label</key>\n\
                 \t<string>{label}</string>\n\
                 \t<key>ProgramArguments</key>\n\
                 \t<array>\n\
                 \t\t<string>{exe}</string>\n\
                 \t\t<string>serve</string>\n\
                 \t\t<string>--home</string>\n\
                 \t\t<string>{home}</string>\n\
                 \t\t<string>--idle-exit-seconds</string>\n\
                 \t\t<string>0</string>\n\
                 \t</array>\n\
                 \t<key>RunAtLoad</key>\n\
                 \t<true/>\n\
                 \t<key>KeepAlive</key>\n\
                 \t<dict>\n\
                 \t\t<key>SuccessfulExit</key>\n\
                 \t\t<false/>\n\
                 \t</dict>\n\
                 \t<key>StandardErrorPath</key>\n\
                 \t<string>{home}/service.log</string>\n\
                 \t<key>StandardOutPath</key>\n\
                 \t<string>{home}/service.log</string>\n\
                 </dict>\n\
                 </plist>\n",
                label = MACOS_LABEL,
                exe = xml_escape(&ctx.exe.to_string_lossy()),
                home = xml_escape(&home),
            );
            let path = plist_path(ctx);
            let domain = format!("gui/{}", ctx.uid.unwrap_or(501));
            Plan {
                commands: vec![
                    // 重裝時先卸掉舊的；還沒裝過會失敗，所以放在 apply 會忽略錯誤的前置步驟之外，改用 bootstrap 覆蓋前的 bootout
                    s(&["launchctl", "bootstrap", &domain, &path.to_string_lossy()]),
                ],
                files: vec![(path, plist)],
                removes: vec![],
                notes: vec![
                    format!(
                        "查看：launchctl print {domain}/{MACOS_LABEL}；紀錄：{home}/service.log 與 {home}/collector.log"
                    ),
                    "macOS 第一次會跳「允許在背景執行」的通知，允許即可。".into(),
                ],
            }
        }
        Platform::Windows => {
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n\
                 <Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n\
                 \x20 <RegistrationInfo>\r\n\
                 \x20   <Description>Coding Lab collector（Claude Code session 追蹤）；由 coding-lab service install 建立</Description>\r\n\
                 \x20 </RegistrationInfo>\r\n\
                 \x20 <Triggers>\r\n\
                 \x20   <LogonTrigger>\r\n\
                 \x20     <Enabled>true</Enabled>\r\n\
                 \x20   </LogonTrigger>\r\n\
                 \x20 </Triggers>\r\n\
                 \x20 <Principals>\r\n\
                 \x20   <Principal id=\"Author\">\r\n\
                 \x20     <LogonType>InteractiveToken</LogonType>\r\n\
                 \x20     <RunLevel>LeastPrivilege</RunLevel>\r\n\
                 \x20   </Principal>\r\n\
                 \x20 </Principals>\r\n\
                 \x20 <Settings>\r\n\
                 \x20   <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\r\n\
                 \x20   <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\r\n\
                 \x20   <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\r\n\
                 \x20   <AllowHardTerminate>true</AllowHardTerminate>\r\n\
                 \x20   <StartWhenAvailable>true</StartWhenAvailable>\r\n\
                 \x20   <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\r\n\
                 \x20   <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\r\n\
                 \x20   <Hidden>true</Hidden>\r\n\
                 \x20   <RestartOnFailure>\r\n\
                 \x20     <Interval>PT1M</Interval>\r\n\
                 \x20     <Count>3</Count>\r\n\
                 \x20   </RestartOnFailure>\r\n\
                 \x20 </Settings>\r\n\
                 \x20 <Actions Context=\"Author\">\r\n\
                 \x20   <Exec>\r\n\
                 \x20     <Command>{exe}</Command>\r\n\
                 \x20     <Arguments>serve --home \"{home}\" --idle-exit-seconds 0</Arguments>\r\n\
                 \x20   </Exec>\r\n\
                 \x20 </Actions>\r\n\
                 </Task>\r\n",
                exe = xml_escape(&ctx.exe.to_string_lossy()),
                home = xml_escape(&home),
            );
            let path = task_xml_path(ctx);
            Plan {
                commands: vec![
                    s(&["schtasks", "/Create", "/TN", WINDOWS_TASK, "/XML", &path.to_string_lossy(), "/F"]),
                    s(&["schtasks", "/Run", "/TN", WINDOWS_TASK]),
                ],
                files: vec![(path, xml)],
                removes: vec![],
                notes: vec![
                    format!("查看：schtasks /Query /TN {WINDOWS_TASK} /V /FO LIST；紀錄：{home}\\collector.log"),
                    "工作排程器在使用者登入時啟動；要在沒人登入時就跑，請在「工作排程器」裡把這個工作改成「不論使用者登入與否均執行」。".into(),
                ],
            }
        }
    }
}

pub fn plan_uninstall(platform: Platform, ctx: &Ctx) -> Plan {
    let removes = files_for(platform, ctx);
    let commands = match platform {
        Platform::Linux => vec![
            s(&["systemctl", "--user", "disable", "--now", LINUX_UNIT]),
            s(&["systemctl", "--user", "daemon-reload"]),
        ],
        Platform::Macos => {
            let domain = format!("gui/{}", ctx.uid.unwrap_or(501));
            vec![s(&[
                "launchctl",
                "bootout",
                &format!("{domain}/{MACOS_LABEL}"),
            ])]
        }
        Platform::Windows => vec![
            s(&["schtasks", "/End", "/TN", WINDOWS_TASK]),
            s(&["schtasks", "/Delete", "/TN", WINDOWS_TASK, "/F"]),
        ],
    };
    Plan {
        files: vec![],
        removes,
        commands,
        notes: vec!["常駐服務已移除；collector 之後仍會由每個 session 的 SessionStart 在背景拉起（閒置 10 分鐘後自動結束）".into()],
    }
}

/// 檔案照 Plan 寫入或刪除（UTF-16 開頭的 XML 用 UTF-16LE＋BOM 寫，工作排程器才讀得懂），然後依序跑指令；
/// 任何一條指令失敗就停，錯誤訊息帶那條指令。刪不存在的檔不算錯。
pub fn apply(
    plan: &Plan,
    run: &mut dyn FnMut(&[String]) -> Result<(), String>,
) -> Result<(), String> {
    for (path, content) in &plan.files {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("建不出 {}：{e}", dir.display()))?;
        }
        let bytes = if content.starts_with("<?xml version=\"1.0\" encoding=\"UTF-16\"?>") {
            let mut b = vec![0xFF, 0xFE];
            for u in content.encode_utf16() {
                b.extend_from_slice(&u.to_le_bytes());
            }
            b
        } else {
            content.as_bytes().to_vec()
        };
        std::fs::write(path, bytes).map_err(|e| format!("寫不了 {}：{e}", path.display()))?;
    }
    for path in &plan.removes {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("刪不掉 {}：{e}", path.display())),
        }
    }
    for cmd in &plan.commands {
        run(cmd).map_err(|e| format!("{} 失敗：{e}", cmd.join(" ")))?;
    }
    Ok(())
}

/// 真的執行一條指令：成功要 exit 0，否則把 stderr 帶回來
pub fn run_command(cmd: &[String]) -> Result<(), String> {
    let (prog, args) = cmd.split_first().ok_or("空的指令")?;
    let out = std::process::Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| format!("執行不了 {prog}：{e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    Err(if err.is_empty() {
        format!("exit {}", out.status.code().unwrap_or(-1))
    } else {
        err.to_string()
    })
}
