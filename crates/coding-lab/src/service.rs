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
/// 工作排程器的資料夾（W-L11）；0.1.0 之前這是放在根目錄的工作名稱
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
    /// Windows：`DOMAIN\\user`（工作排程器的 `<UserId>`，#719）；其他平台 None
    pub user: Option<String>,
}

impl Ctx {
    /// 從真正的環境組出來；`home` 用設定算出的資料目錄
    pub fn detect(home: &Path) -> Result<Ctx, String> {
        let exe = std::env::current_exe().map_err(|e| format!("找不到自己的路徑：{e}"))?;
        let user_home =
            crate::config::user_home().ok_or("沒有 HOME／USERPROFILE，不知道家目錄在哪")?;
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| user_home.join(".config"));
        // 只有 macOS 用得到（launchctl 的 gui/<uid>）；Windows 的 Git Bash 裡真的有 id.exe，每次都會開一個（E-B8）
        let uid = if cfg!(target_os = "macos") {
            std::process::Command::new("id")
                .arg("-u")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.trim().parse().ok())
        } else {
            None
        };
        let user = if cfg!(windows) {
            std::env::var("USERNAME")
                .ok()
                .filter(|u| !u.is_empty())
                .map(
                    |u| match std::env::var("USERDOMAIN").ok().filter(|d| !d.is_empty()) {
                        Some(d) => format!("{d}\\{u}"),
                        None => u,
                    },
                )
        } else {
            None
        };
        Ok(Ctx {
            exe,
            home: home.to_path_buf(),
            config_home,
            user_home,
            uid,
            user,
        })
    }
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

/// unit 檔裡的一個路徑值（R-15）：systemd 會展開 `%` 指定符，所以寫成 `%%`；Exec 行還會展開 `$` 變數，
/// 寫成 `$$`（Environment= 不展開 `$`）。Exec 行用空白分隔參數：有空白、引號、反斜線、`;` 的加引號，
/// 裡面的 `\` 與 `"` 跳脫。
fn systemd_value(p: &Path, exec: bool) -> String {
    let mut t = p.to_string_lossy().replace('%', "%%");
    if exec {
        t = t.replace('$', "$$");
    }
    if t.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';')) {
        format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        t
    }
}

/// Windows 命令列的一個參數（W-M6）：一律加引號；照 `CommandLineToArgvW` 的規則，接在 `"` 前面的
/// 反斜線（含結尾的）要加倍，否則 `"C:\x\"` 的引號會被跳脫、後面的參數全黏進來。
fn windows_arg(t: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in t.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
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
                 StartLimitIntervalSec=0\n\
                 \n\
                 [Service]\n\
                 Type=simple\n\
                 Environment=CODING_LAB_HOME={env_home}\n\
                 ExecStart={exe} serve --home {home} --idle-exit-seconds 0\n\
                 ExecStop={exe} stop\n\
                 Restart=on-failure\n\
                 RestartSec=2\n\
                 \n\
                 [Install]\n\
                 WantedBy=default.target\n",
                exe = systemd_value(&ctx.exe, true),
                home = systemd_value(&ctx.home, true),
                env_home = systemd_value(&ctx.home, false),
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
                 {user_trigger}\
                 \x20   </LogonTrigger>\r\n\
                 \x20 </Triggers>\r\n\
                 \x20 <Principals>\r\n\
                 \x20   <Principal id=\"Author\">\r\n\
                 {user_principal}\
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
                 \x20     <Command>%SystemRoot%\\System32\\conhost.exe</Command>\r\n\
                 \x20     <Arguments>--headless {exe} serve --home {home} --idle-exit-seconds 0</Arguments>\r\n\
                 \x20   </Exec>\r\n\
                 \x20 </Actions>\r\n\
                 </Task>\r\n",
                // L-12：console 程式直接當 Exec，登入時會開出主控台視窗、關掉就殺掉 collector；
                // 經 headless 的 conhost 跑沒有視窗（`<Hidden>` 只是在工作排程器的清單裡隱藏）
                exe = xml_escape(&windows_arg(&ctx.exe.to_string_lossy())),
                home = xml_escape(&windows_arg(&home)),
                // 一般使用者只能建「自己登入時、以自己身分跑」的工作：trigger 與 principal 都要填目前使用者（#719）
                user_trigger = ctx
                    .user
                    .as_deref()
                    .map(|u| format!("      <UserId>{}</UserId>\r\n", xml_escape(u)))
                    .unwrap_or_default(),
                user_principal = ctx
                    .user
                    .as_deref()
                    .map(|u| format!("      <UserId>{}</UserId>\r\n", xml_escape(u)))
                    .unwrap_or_default(),
            );
            let path = task_xml_path(ctx);
            let task = windows_task(ctx);
            Plan {
                commands: vec![
                    s(&["schtasks", "/Create", "/TN", &task, "/XML", &path.to_string_lossy(), "/F"]),
                    s(&["schtasks", "/Run", "/TN", &task]),
                ],
                files: vec![(path, xml)],
                removes: vec![],
                notes: vec![
                    format!("查看：schtasks /Query /TN {task} /V /FO LIST；紀錄：{home}\\collector.log"),
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
            s(&["schtasks", "/End", "/TN", &windows_task(ctx)]),
            s(&["schtasks", "/Delete", "/TN", &windows_task(ctx), "/F"]),
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

/// 把裝好的常駐服務叫起來的指令（R-6）：`coding-lab stop` 讓它正常結束，三個平台都不算失敗、不會自己重啟。
pub fn start_command(platform: Platform, ctx: &Ctx) -> Vec<String> {
    match platform {
        Platform::Linux => s(&["systemctl", "--user", "start", LINUX_UNIT]),
        Platform::Macos => s(&[
            "launchctl",
            "kickstart",
            &format!("gui/{}/{MACOS_LABEL}", ctx.uid.unwrap_or(501)),
        ]),
        Platform::Windows => s(&["schtasks", "/Run", "/TN", &windows_task(ctx)]),
    }
}

/// 問系統「這個服務有沒有登記」的指令（#719）：install 失敗後檔案可能還在，所以 status 以這個為準
pub fn status_command(platform: Platform, ctx: &Ctx) -> Vec<String> {
    match platform {
        Platform::Linux => s(&["systemctl", "--user", "is-enabled", LINUX_UNIT]),
        Platform::Macos => s(&[
            "launchctl",
            "print",
            &format!("gui/{}/{MACOS_LABEL}", ctx.uid.unwrap_or(501)),
        ]),
        Platform::Windows => s(&["schtasks", "/Query", "/TN", &windows_task(ctx)]),
    }
}

/// `run` 執行 [`status_command`]（成功回 stdout）：查得到就是已安裝；Linux 另外要 `enabled`
pub fn query_installed(
    platform: Platform,
    ctx: &Ctx,
    run: &mut dyn FnMut(&[String]) -> Result<String, String>,
) -> bool {
    match run(&status_command(platform, ctx)) {
        Ok(out) => platform != Platform::Linux || out.trim().starts_with("enabled"),
        Err(_) => false,
    }
}

/// install：寫檔、登記；登記失敗就把剛寫的檔收回，並呼叫 `restore`（把先前為了讓出埠而停掉的 collector 拉回來，#719），
/// 失敗原因交給 `log` 寫進 collector.log（#726：只印 stderr 的話事後查不到）
pub fn install(
    plan: &Plan,
    run: &mut dyn FnMut(&[String]) -> Result<(), String>,
    restore: &mut dyn FnMut(),
    log: &mut dyn FnMut(&str),
) -> Result<(), String> {
    match apply(plan, run) {
        Ok(()) => Ok(()),
        Err(e) => {
            log(&format!("service：install 沒有裝成：{e}"));
            for (p, _) in &plan.files {
                let _ = std::fs::remove_file(p);
            }
            restore();
            Err(e)
        }
    }
}

/// Windows 的 OEM code page 編號 → WHATWG 編碼標籤（`encoding_rs` 認得的）；不認得回 None。
pub fn code_page_label(cp: u32) -> Option<&'static str> {
    Some(match cp {
        65001 => "utf-8",
        950 => "big5",
        936 => "gbk",
        932 => "shift_jis",
        949 => "euc-kr",
        874 => "windows-874",
        1250 => "windows-1250",
        1251 => "windows-1251",
        1252 => "windows-1252",
        1253 => "windows-1253",
        1254 => "windows-1254",
        1255 => "windows-1255",
        1256 => "windows-1256",
        1257 => "windows-1257",
        1258 => "windows-1258",
        _ => return None,
    })
}

/// 用指定的 code page 解碼（Windows 子程序的 stderr／stdout，#724）；不認得的 code page 回 None。
#[cfg(windows)]
pub fn decode_with_code_page(bytes: &[u8], cp: u32) -> Option<String> {
    let enc = encoding_rs::Encoding::for_label(code_page_label(cp)?.as_bytes())?;
    let (text, _, had_errors) = enc.decode(bytes);
    (!had_errors).then(|| text.trim().to_string())
}

/// 非 Windows 的子程序輸出本來就是 UTF-8，不需要 code page 解碼：encoding_rs 只在 Windows 編（#758 E-B9）。
#[cfg(not(windows))]
pub fn decode_with_code_page(_bytes: &[u8], _cp: u32) -> Option<String> {
    None
}

/// 這台 Windows 主控台的 OEM code page：`reg query` 的 `OEMCP` 值（不用 unsafe 的 Win32 呼叫）。非 Windows 回 None。
fn oem_code_page() -> Option<u32> {
    if !cfg!(windows) {
        return None;
    }
    static CP: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *CP.get_or_init(|| {
        let out = std::process::Command::new("reg")
            .args([
                "query",
                r"HKLM\SYSTEM\CurrentControlSet\Control\Nls\CodePage",
                "/v",
                "OEMCP",
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.split_whitespace()
            .last()
            .and_then(|v| v.parse::<u32>().ok())
    })
}

/// 子程序輸出轉成文字：合法 UTF-8 照用；不是（Windows 的 schtasks 用 OEM code page，例如 cp950）就依這台的
/// code page 解碼，解不了才 lossy。回報 `true` 表示最後是 lossy 的。
pub fn decode_output(bytes: &[u8]) -> (String, bool) {
    decode_output_with(bytes, oem_code_page())
}

/// `decode_output` 的本體，code page 由呼叫端給（測試不受執行機器的 OEM code page 影響）。
pub fn decode_output_with(bytes: &[u8], code_page: Option<u32>) -> (String, bool) {
    if let Ok(t) = std::str::from_utf8(bytes) {
        return (t.trim().to_string(), false);
    }
    if let Some(t) = code_page.and_then(|cp| decode_with_code_page(bytes, cp)) {
        return (t, false);
    }
    (String::from_utf8_lossy(bytes).trim().to_string(), true)
}

/// 失敗的子程序輸出變成錯誤訊息。`quiet`：預期中的失敗（查詢「沒裝」）不寫 log。
fn output_error(prog: &str, out: &std::process::Output, quiet: bool) -> String {
    let raw = if out.stderr.is_empty() {
        &out.stdout
    } else {
        &out.stderr
    };
    let (text, lossy) = decode_output(raw);
    if text.is_empty() {
        return format!("exit {}", out.status.code().unwrap_or(-1));
    }
    if lossy && !quiet {
        // 連這台的 code page 都解不了：原始位元組寫進 log（CLI 子命令要先 logging::to_file 才會進檔案）
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        crate::logging::log(&format!(
            "service：{prog} 的輸出解不了（原始十六進位）：{hex}"
        ));
        return format!("{text}（原始訊息解不了，位元組已寫進 collector.log）");
    }
    text
}

/// 真的執行一條指令：成功要 exit 0，否則把 stderr 帶回來（並寫 log）
pub fn run_command(cmd: &[String]) -> Result<(), String> {
    run_command_capture(cmd).map(|_| ())
}

/// 同 [`run_command`]，成功時回 stdout
fn run_command_capture(cmd: &[String]) -> Result<String, String> {
    capture(cmd, false)
}

/// 同 [`run_command_capture`]，但失敗是預期中的（status 查詢、uninstall 時工作早就不在）：不寫 log、不印十六進位（#724）
pub fn run_command_capture_quiet(cmd: &[String]) -> Result<String, String> {
    capture(cmd, true)
}

fn capture(cmd: &[String], quiet: bool) -> Result<String, String> {
    let (prog, args) = cmd.split_first().ok_or("空的指令")?;
    let out = std::process::Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| format!("執行不了 {prog}：{e}"))?;
    if out.status.success() {
        return Ok(decode_output(&out.stdout).0);
    }
    Err(output_error(prog, &out, quiet))
}

/// Windows 的工作名稱（W-L11）：放在 `\CodingLab\` 資料夾、以使用者命名。工作排程器根目錄的名稱整台機器共用，
/// 固定叫 `CodingLab` 時別的帳號裝不起來（存取被拒），系統管理員的 `/F` 還會蓋掉別人的。
pub fn windows_task(ctx: &Ctx) -> String {
    let user = ctx
        .user
        .as_deref()
        .and_then(|u| u.rsplit('\\').next())
        .unwrap_or("");
    let name: String = user
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let name = if name.trim().is_empty() {
        "default"
    } else {
        name.trim()
    };
    format!("\\{WINDOWS_TASK}\\{name}")
}

/// 已經裝好的常駐服務用的是哪個 home（W-L11）：Linux、macOS 讀 install 寫的檔（位置和 home 無關），
/// Windows 問工作排程器要這個使用者那個工作的 XML。沒裝、讀不到都是 None。
pub fn existing_home(
    platform: Platform,
    ctx: &Ctx,
    run: &mut dyn FnMut(&[String]) -> Result<String, String>,
) -> Option<PathBuf> {
    match platform {
        Platform::Linux => home_from_unit(&std::fs::read_to_string(unit_path(ctx)).ok()?),
        Platform::Macos => home_from_plist(&std::fs::read_to_string(plist_path(ctx)).ok()?),
        Platform::Windows => home_from_task_xml(
            &run(&s(&[
                "schtasks",
                "/Query",
                "/TN",
                &windows_task(ctx),
                "/XML",
            ]))
            .ok()?,
        ),
    }
}

/// 舊版（0.1.0 之前）放在工作排程器根目錄的 `CodingLab`：home 和這次的相同才刪（是自己的），
/// 不同的可能是別的帳號的，不動。回傳要跑的刪除指令。
pub fn legacy_windows_task(
    ctx: &Ctx,
    run: &mut dyn FnMut(&[String]) -> Result<String, String>,
) -> Option<Vec<String>> {
    let xml = run(&s(&["schtasks", "/Query", "/TN", WINDOWS_TASK, "/XML"])).ok()?;
    let home = home_from_task_xml(&xml)?;
    same_home(&home, &ctx.home).then(|| s(&["schtasks", "/Delete", "/TN", WINDOWS_TASK, "/F"]))
}

fn same_home(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// install／uninstall 前的檢查（W-L11）：已經有服務、但用的是別的 home 時拒絕，說出原本的 home；`force` 才照做。
/// 同一個帳號拿測試用的 CODING_LAB_HOME 試服務時，不會把正式的服務換掉或移除。
pub fn check_existing(existing: Option<&Path>, home: &Path, force: bool) -> Result<(), String> {
    match existing {
        Some(old) if !force && !same_home(old, home) => Err(format!(
            "這個帳號已經裝了常駐服務，用的資料目錄是 {}，不是目前的 {}；確定要換成目前的請加 --force（或用原本的 home 執行 service uninstall）",
            old.display(),
            home.display()
        )),
        _ => Ok(()),
    }
}

/// unit 檔的 `Environment=CODING_LAB_HOME=…`，還原 [`systemd_value`] 的引號、跳脫與 `%%`。
pub fn home_from_unit(text: &str) -> Option<PathBuf> {
    let v = text
        .lines()
        .find_map(|l| l.strip_prefix("Environment=CODING_LAB_HOME="))?;
    let v = match v.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => {
            let mut out = String::new();
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                out.push(if c == '\\' {
                    chars.next().unwrap_or('\\')
                } else {
                    c
                });
            }
            out
        }
        None => v.to_string(),
    };
    Some(PathBuf::from(v.replace("%%", "%")))
}

/// plist 的 ProgramArguments 裡 `--home` 的下一個 `<string>`。
pub fn home_from_plist(text: &str) -> Option<PathBuf> {
    let after = text.split_once("<string>--home</string>")?.1;
    let v = after.split_once("<string>")?.1.split_once("</string>")?.0;
    Some(PathBuf::from(xml_unescape(v)))
}

/// 工作 XML 的 `<Arguments>` 照 Windows 命令列規則拆開，取 `--home` 的下一個。
pub fn home_from_task_xml(text: &str) -> Option<PathBuf> {
    let args = text
        .split_once("<Arguments>")?
        .1
        .split_once("</Arguments>")?
        .0;
    let args = split_windows_args(&xml_unescape(args));
    let i = args.iter().position(|a| a == "--home")?;
    args.get(i + 1).map(PathBuf::from)
}

fn xml_unescape(t: &str) -> String {
    t.replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// [`windows_arg`] 的反向：`CommandLineToArgvW` 的規則（`2n` 個反斜線＋引號 → `n` 個反斜線、切換引號；
/// `2n+1` 個 → `n` 個反斜線＋字面的引號；其他地方的反斜線照原樣）。
fn split_windows_args(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut started, mut slashes) = (false, false, 0usize);
    for c in line.chars() {
        match c {
            '\\' => {
                slashes += 1;
                started = true;
            }
            '"' => {
                cur.extend(std::iter::repeat_n('\\', slashes / 2));
                if slashes % 2 == 1 {
                    cur.push('"');
                } else {
                    quoted = !quoted;
                }
                slashes = 0;
                started = true;
            }
            _ => {
                cur.extend(std::iter::repeat_n('\\', slashes));
                slashes = 0;
                if c.is_whitespace() && !quoted {
                    if started {
                        args.push(std::mem::take(&mut cur));
                        started = false;
                    }
                } else {
                    cur.push(c);
                    started = true;
                }
            }
        }
    }
    cur.extend(std::iter::repeat_n('\\', slashes));
    if started {
        args.push(cur);
    }
    args
}
