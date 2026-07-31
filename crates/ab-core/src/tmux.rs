//! `TmuxClient` trait + `SubprocessTmux` 實作。**以裸名 `tmux` 經 PATH
//! spawn**（測試 shim 攔截前提，tests/run-tests.sh:91-93／架構 §2 tmux 列）；
//! argv 陣列傳參，不組字串餵 shell。

use crate::config;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
    /// 回 `None`＝子行程根本起不來（PATH 沒有 tmux、fork 失敗）**或逾時被殺**
    /// （ENV-TMUX-1），與「tmux 跑了但回非零」是兩件事——後者呼叫端要看得到
    /// stderr。
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

    /// `tmux display -pt <pane> '#{pane_in_mode}'`：pane 是否停在 tmux 的
    /// copy-mode／view-mode。**失敗回 `None`，呼叫端 MUST fail-closed**
    /// （AB-COPYMODE-1，見 `notify_pane`）。
    ///
    /// 這是 Rust 獨有的關卡（bash 正本自 M4 凍結時沒有）。
    fn pane_in_mode(&self, pane: &str) -> Option<bool>;

    /// 對齊 `tmux send-keys -t <pane> <keys>`；成功回 `true`。
    ///
    /// **實作 MUST 有逾時**：copy-mode 中的 pane 會讓 send-keys 永不返回
    /// （AB-COPYMODE-1 實測），沒有逾時就等於整個 `send` 指令被鎖死。
    fn send_keys(&self, pane: &str, keys: &str) -> bool;
}

pub struct SubprocessTmux;

impl TmuxClient for SubprocessTmux {
    fn available(&self) -> bool {
        command_exists("tmux")
    }

    fn exec(&self, args: &[&str]) -> Option<TmuxOutput> {
        // 逾時與起不來同一終態 `None`：TUI read model（tui-design §4 bounded-read
        // 硬條款）與 spawn 生命週期共用這條路，卡住的 tmux 不得凍結呼叫端。
        let out = run_bounded(args)?;
        Some(TmuxOutput {
            status_ok: out.status_ok,
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    fn resolve_pane(&self, target: &str) -> Option<String> {
        let out = run_bounded(&["display", "-pt", target, "#{pane_id}"])?;
        if !out.status_ok {
            return None;
        }
        let s = out.stdout.trim_end_matches('\n');
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }

    fn pane_exists(&self, pane: &str) -> bool {
        match run_bounded(&["list-panes", "-a", "-F", "#{pane_id}"]) {
            // `grep -Fx`：整行相等，不是子字串比對
            Some(out) if out.status_ok => out.stdout.lines().any(|l| l == pane),
            _ => false,
        }
    }

    fn capture_pane(&self, pane: &str) -> Option<String> {
        // 畫面文字只用來做特徵字串比對，非 UTF-8 位元組以 lossy 轉換即可
        // （這裡不是 payload 路徑，架構 §3 的 byte 保真紅線不適用）。
        let out = run_bounded(&["capture-pane", "-pJ", "-t", pane])?;
        if out.status_ok {
            Some(out.stdout)
        } else {
            None
        }
    }

    fn pane_in_mode(&self, pane: &str) -> Option<bool> {
        let out = run_bounded(&["display", "-pt", pane, "#{pane_in_mode}"])?;
        if !out.status_ok {
            return None;
        }
        // `#{pane_in_mode}` 只會是 `0`／`1`；其餘一律當「讀不出來」而非「不在
        // mode」——這條路徑的 `None` 會 fail-closed，猜 false 才是危險方向。
        match out.stdout.trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        }
    }

    fn send_keys(&self, pane: &str, keys: &str) -> bool {
        // 逾時：子行程已被殺，當作送鍵失敗（呼叫端走 notify-failed 降級，
        // 訊息仍在 mailbox）。
        matches!(run_bounded(&["send-keys", "-t", pane, keys]), Some(out) if out.status_ok)
    }
}

/// `run_bounded` 的結果。stderr 只有 `exec` 的呼叫端會看（despawn 把它併進
/// die 訊息），其餘路徑忽略。
struct BoundedOutput {
    status_ok: bool,
    stdout: String,
    stderr: String,
}

/// 有逾時上限的 tmux 呼叫，**本模組所有 tmux 子行程一律走這裡**（原先只涵蓋
/// 通知熱路徑；TUI 動工前依 tui-design §4 bounded-read 硬條款補齊 `exec`／
/// `resolve_pane`——任何一條無界查詢都足以凍結整個 UI 刷新迴圈）。
///
/// 只把逾時加在 `send-keys` 不夠（跨廠複核 2026-07-31 finding 1）：那只擋掉實測
/// 到的那一個卡點，而 `notify_pane` 在送鍵之前還會跑 `list-panes`、`display`、
/// `capture-pane` 三種查詢——tmux server 或 socket 一旦卡住，`send` 照樣無限
/// 等待，`AGENT_BRIDGE_TMUX_TIMEOUT` 形同不存在。契約（hooks.md
/// HOOK-NOTIFY-3）承諾的是「整條通知路徑不被 tmux 鎖死」，故上限套在這一層。
///
/// stdout／stderr 各以獨立執行緒讀取而非等子行程結束後再讀：輸出量超過 pipe
/// buffer 時，子行程會卡在寫入、父行程卡在等待，兩邊互等——那正是這個函式要
/// 消滅的終態。
///
/// 逾時回 `None`：與「tmux 起不來」同一個終態，呼叫端一律 fail-closed。
fn run_bounded(args: &[&str]) -> Option<BoundedOutput> {
    let mut child = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let spawn_reader = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        pipe.map(|mut out| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut out, &mut buf);
                buf
            })
        })
    };
    let out_reader = spawn_reader(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let err_reader = spawn_reader(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let status = wait_with_timeout(&mut child, config::tmux_timeout());
    // 子行程已結束或已被殺，pipe 因此 EOF，讀取緒必定收斂——join 不會卡住。
    let out_buf = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let err_buf = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let status = status?;
    Some(BoundedOutput {
        status_ok: status.success(),
        // tmux 的輸出是 pane id／視窗清單／選項值這類受控文字，非 payload
        // 路徑，lossy 轉換不觸及架構 §3 的 byte 保真紅線。
        stdout: String::from_utf8_lossy(&out_buf).into_owned(),
        stderr: String::from_utf8_lossy(&err_buf).into_owned(),
    })
}

/// `Command` 沒有內建的「等 N 秒否則殺掉」，這裡以 `try_wait` 輪詢補上。
///
/// `timeout` 為 `None`＝不設限（`AGENT_BRIDGE_TMUX_TIMEOUT=0` 的逃生口），
/// 回到加逾時之前的行為。逾時回 `None`：先 `kill` 再 `wait` 收屍，不留殭屍。
///
/// 輪詢間隔取 20ms：正常的 tmux 查詢是毫秒級，這個粒度不會讓常見路徑多等一
/// 個可感知的量，又不至於在逾時窗裡空轉太多次。
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Option<Duration>,
) -> Option<std::process::ExitStatus> {
    let Some(timeout) = timeout else {
        return child.wait().ok();
    };
    // `checked_add`：加不出期限就當「不設限」而非 panic。上限已在
    // `config::tmux_timeout` 夾過，這裡是不信任呼叫端的第二道
    // （跨廠複核 2026-07-31 finding 2：溢位的 `Instant + Duration` 會讓通知
    // 階段 panic，而那是任務已建立之後——比不設限難救得多）。
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        return child.wait().ok();
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            // EINTR 只是輪詢被訊號打斷，子行程沒事——當成逾時會誤殺一個正常的
            // tmux，做出假的 notify-failed（跨廠複核 2026-07-31 finding 5）。
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // 其餘錯誤＝等不到狀態，殺掉當逾時處理，避免呼叫端無限等下去
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    /// AB-COPYMODE-1 的兜底：不會返回的子行程 MUST 被逾時砍掉，而不是讓呼叫端
    /// 無限等下去。測試本身跑得完就是斷言——不對牆鐘秒數作數值比對（那會做出
    /// 隨機紅的 flake）。
    #[test]
    fn a_hanging_child_is_killed_at_the_deadline() {
        let mut child = Command::new("sleep")
            .arg("300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep 應可 spawn");
        assert!(wait_with_timeout(&mut child, Some(Duration::from_millis(100))).is_none());
    }

    /// 正常結束的子行程照樣要回真實退出狀態，不得被逾時路徑吃掉。
    #[test]
    fn a_prompt_child_returns_its_real_status() {
        let mut child = Command::new("true")
            .stdin(Stdio::null())
            .spawn()
            .expect("true 應可 spawn");
        let status = wait_with_timeout(&mut child, Some(Duration::from_secs(30)));
        assert!(status.expect("應在期限內結束").success());
    }

    /// `AGENT_BRIDGE_TMUX_TIMEOUT=0` 的逃生口：不設限時退回單純 `wait`。
    #[test]
    fn no_timeout_still_waits_for_the_child() {
        let mut child = Command::new("true")
            .stdin(Stdio::null())
            .spawn()
            .expect("true 應可 spawn");
        assert!(
            wait_with_timeout(&mut child, None)
                .expect("應收到狀態")
                .success()
        );
    }
}
