//! `TmuxClient` trait + `SubprocessTmux` 實作。**以裸名 `tmux` 經 PATH
//! spawn**（測試 shim 攔截前提，tests/run-tests.sh:91-93／架構 §2 tmux 列）；
//! argv 陣列傳參，不組字串餵 shell。

use std::path::Path;
use std::process::{Command, Stdio};

/// 一次 tmux 子行程呼叫的結果。`status_ok` 對映 shell 的退出碼是否為 0；
/// stdout／stderr 各自捕捉——despawn 的兩處查詢把 stderr 併進 die 訊息
/// （`2>&1`，bin/agent-bridge:1523、1564），其餘一律丟棄。
pub struct TmuxOutput {
    pub status_ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl TmuxOutput {
    /// bash 慣用的 `out="$(tmux … 2>/dev/null)" || …`：成功才取 stdout
    /// （去掉尾端換行），失敗回 `None`。
    pub fn ok_stdout(self) -> Option<String> {
        if self.status_ok {
            Some(self.stdout.trim_end_matches('\n').to_string())
        } else {
            None
        }
    }
}

pub trait TmuxClient {
    /// 泛用逃生口：以 argv 陣列呼叫 tmux，回傳退出狀態與兩條輸出。
    /// spawn 生命週期用到十來種 tmux 子命令（new-window／split-window／
    /// if-shell／set-option／show-options／select-*／list-windows…），逐個
    /// 長成 trait 方法只會讓這層變成 tmux CLI 的鏡像；語意留在 `spawn`
    /// 模組、這裡只負責「把 argv 送出去、把輸出讀回來」。
    ///
    /// 回 `None`＝子行程根本起不來（PATH 沒有 tmux、fork 失敗），與
    /// 「tmux 跑了但回非零」是兩件事——後者呼叫端要看得到 stderr。
    ///
    /// 架構 §5：先讀盡 stdout/stderr 再 `wait()`（`Command::output` 內建
    /// 如此），避免 pipe buffer 滿載互等。
    fn exec(&self, args: &[&str]) -> Option<TmuxOutput>;

    /// 對齊 bash `command -v tmux`：只驗證 PATH 上有可執行檔，**不**呼叫它
    /// ——測試的 failshim 本身是一個會失敗的可執行腳本，`command -v` 視為
    /// 「存在」，之後呼叫才失敗（另一條 die 訊息路徑）。
    fn available(&self) -> bool;

    /// 對齊 bash `tmux display -pt <target> '#{pane_id}'`：解析成功回傳
    /// pane id；失敗（非零 exit、空輸出）回 `None`。
    fn resolve_pane(&self, target: &str) -> Option<String>;

    /// 對齊 `tmux list-panes -a -F '#{pane_id}' | grep -Fx "$pane"`：pane 是否
    /// 還活著。tmux 呼叫失敗一律回 `false`（notify_pane:334 的 fail-closed）。
    fn pane_exists(&self, pane: &str) -> bool;

    /// 對齊 `tmux capture-pane -pJ -t <pane>`（`-J` 把軟折行接回原樣）。
    /// **失敗回 `None`，呼叫端 MUST fail-closed**：capture 失敗＝無法確認 pane
    /// 狀態，放行送鍵等於整條權限框防線被略過（notify_pane:339-341）。
    fn capture_pane(&self, pane: &str) -> Option<String>;

    /// 對齊 `tmux send-keys -t <pane> <keys>`；成功回 `true`。
    fn send_keys(&self, pane: &str, keys: &str) -> bool;
}

pub struct SubprocessTmux;

impl TmuxClient for SubprocessTmux {
    fn available(&self) -> bool {
        command_exists("tmux")
    }

    fn exec(&self, args: &[&str]) -> Option<TmuxOutput> {
        let out = Command::new("tmux")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        Some(TmuxOutput {
            status_ok: out.status.success(),
            // tmux 的輸出是 pane id／視窗清單／選項值這類受控文字，非 payload
            // 路徑，lossy 轉換不觸及架構 §3 的 byte 保真紅線。
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
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

    fn pane_exists(&self, pane: &str) -> bool {
        let out = match Command::new("tmux")
            .args(["list-panes", "-a", "-F", "#{pane_id}"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            Ok(o) => o,
            Err(_) => return false,
        };
        if !out.status.success() {
            return false;
        }
        // `grep -Fx`：整行相等，不是子字串比對
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l == pane)
    }

    fn capture_pane(&self, pane: &str) -> Option<String> {
        let out = Command::new("tmux")
            .args(["capture-pane", "-pJ", "-t", pane])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // 畫面文字只用來做特徵字串比對，非 UTF-8 位元組以 lossy 轉換即可
        // （這裡不是 payload 路徑，架構 §3 的 byte 保真紅線不適用）。
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn send_keys(&self, pane: &str, keys: &str) -> bool {
        Command::new("tmux")
            .args(["send-keys", "-t", pane, keys])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// PATH 掃描版 `which`：不執行候選檔案，只檢查「是檔案且具備任一可執行位元」
/// （Unix）。非 Unix 平台退化為只檢查檔案存在。
pub fn command_exists(name: &str) -> bool {
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
