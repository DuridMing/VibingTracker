//! `collector.log`（#655）：collector 自己寫檔並輪替，10 MB × 5。
//!
//! stderr 被 `ensure` 導到檔案後，程序自己換不了 stderr 指向的檔案，所以 log 改由這裡寫；
//! stderr 是終端機時（開發時前景跑）照舊印到畫面。

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const LOG_KEEP: usize = 5;

/// 超過 `max` bytes 就輪替成 `.1`…`.keep`，最舊的丟掉。
pub struct RotatingLog {
    path: PathBuf,
    max: u64,
    keep: usize,
    file: File,
    len: u64,
}

impl RotatingLog {
    pub fn open(path: &Path, max: u64, keep: usize) -> io::Result<RotatingLog> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(RotatingLog {
            path: path.to_path_buf(),
            max,
            keep,
            file,
            len,
        })
    }

    /// 寫一行（自動補換行）。
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let n = line.len() as u64 + 1;
        if self.len > 0 && self.len + n > self.max {
            self.rotate()?;
        }
        let mut buf = Vec::with_capacity(n as usize);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        self.file.write_all(&buf)?;
        self.len += n;
        Ok(())
    }

    /// `.keep-1` → `.keep`（蓋掉最舊的）…`.1` → `.2`、目前的 → `.1`，再開新檔。
    fn rotate(&mut self) -> io::Result<()> {
        let numbered = |i: usize| {
            let mut p = self.path.clone().into_os_string();
            p.push(format!(".{i}"));
            PathBuf::from(p)
        };
        for i in (1..self.keep).rev() {
            match std::fs::rename(numbered(i), numbered(i + 1)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        std::fs::rename(&self.path, numbered(1))?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.len = 0;
        Ok(())
    }
}

static SINK: OnceLock<Mutex<RotatingLog>> = OnceLock::new();

/// 之後的 [`log`] 寫到這個檔（只第一次呼叫有效）。
pub fn to_file(path: &Path) -> io::Result<()> {
    let log = RotatingLog::open(path, LOG_MAX_BYTES, LOG_KEEP)?;
    let _ = SINK.set(Mutex::new(log));
    Ok(())
}

/// 寫一行 log：有設定檔案就寫檔（輪替），否則印到 stderr。寫不進去就算了（不變式 4）。
pub fn log(msg: &str) {
    let line = format!("[{}] coding-lab：{msg}", crate::now_ms());
    match SINK.get() {
        Some(sink) => {
            let _ = sink
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .write_line(&line);
        }
        None => eprintln!("{line}"),
    }
}

/// 短命的子命令（ensure）直接追加一行到 `path`，同樣會輪替。
pub fn append(path: &Path, msg: &str) {
    if let Ok(mut log) = RotatingLog::open(path, LOG_MAX_BYTES, LOG_KEEP) {
        let _ = log.write_line(&format!("[{}] coding-lab：{msg}", crate::now_ms()));
    }
}
