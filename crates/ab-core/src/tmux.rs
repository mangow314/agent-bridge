//! `TmuxClient` trait + `SubprocessTmux` 實作。**以裸名 `tmux` 經 PATH
//! spawn**（測試 shim 攔截前提，tests/run-tests.sh:91-93／架構 §2 tmux 列）；
//! argv 陣列傳參，不組字串餵 shell。

use std::path::Path;
use std::process::{Command, Stdio};

pub trait TmuxClient {
    /// 對齊 bash `command -v tmux`：只驗證 PATH 上有可執行檔，**不**呼叫它
    /// ——測試的 failshim 本身是一個會失敗的可執行腳本，`command -v` 視為
    /// 「存在」，之後呼叫才失敗（另一條 die 訊息路徑）。
    fn available(&self) -> bool;

    /// 對齊 bash `tmux display -pt <target> '#{pane_id}'`：解析成功回傳
    /// pane id；失敗（非零 exit、空輸出）回 `None`。
    fn resolve_pane(&self, target: &str) -> Option<String>;
}

pub struct SubprocessTmux;

impl TmuxClient for SubprocessTmux {
    fn available(&self) -> bool {
        command_exists("tmux")
    }

    fn resolve_pane(&self, target: &str) -> Option<String> {
        let out = Command::new("tmux")
            .args(["display", "-pt", target, "#{pane_id}"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout);
        let s = s.trim_end_matches('\n');
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }
}

/// PATH 掃描版 `which`：不執行候選檔案，只檢查「是檔案且具備任一可執行位元」
/// （Unix）。非 Unix 平台退化為只檢查檔案存在。
fn command_exists(name: &str) -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    std::env::split_paths(&path).any(|dir| is_executable_file(&dir.join(name)))
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}
