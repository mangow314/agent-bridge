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

    /// **尾行預覽的專用取得路徑**（tui-design §9 P4.7 切片 D）。
    ///
    /// 與 `capture_pane` 分開是刻意的：那一條是 blocker 軸在用的（每 2s 一輪、
    /// 抓當前畫面），語意與界都不同，改它會波及 BLOCKER 軸。這一條的三個界
    /// （history 行數／byte／時間）都成立於**取得路徑本身**：
    /// - 行：`-S -<history_lines>`＝向後要求這麼多行 **history**。實際取得約
    ///   `history_lines + pane 高度`（可見區一律在內），**不是**「總共只取 n
    ///   行」——這一項的作用是「不撈整份 scrollback」，不是總量上限
    /// - byte：讀取迴圈分塊累積，超過就停讀並殺掉子行程（**不得** `read_to_end`）
    ///   ——**總量的硬上限是這一項**
    /// - 時間：呼叫端給的 deadline，不吃 `AGENT_BRIDGE_TMUX_TIMEOUT=0` 的
    ///   無限逃生口
    ///
    /// 逾時／起不來／非零退出一律 `None`（fail-closed，同本 trait 其餘查詢）。
    ///
    /// **有預設實作（回 `None`）**：這是本 trait 唯一有預設的方法。理由是它只
    /// 服務 TUI 的一條 one-shot 路徑，而 repo 裡另有五份為 spawn／notify 寫的
    /// 假件——讓它們一律回「查不出來」比逼每一份都編一個尾行出來誠實，也不會
    /// 因為新增一個方法就動到那幾條路徑的測試。真正的實作在 `SubprocessTmux`，
    /// 要驗參數的假件自己覆寫它。
    fn capture_pane_tail(&self, _pane: &str, _bounds: TailBounds) -> Option<TailCapture> {
        None
    }

    /// 對齊 `tmux send-keys -t <pane> <keys>`；成功回 `true`。
    ///
    /// **實作 MUST 有逾時**：copy-mode 中的 pane 會讓 send-keys 永不返回
    /// （AB-COPYMODE-1 實測），沒有逾時就等於整個 `send` 指令被鎖死。
    fn send_keys(&self, pane: &str, keys: &str) -> bool;
}

/// 尾行預覽的三重界。值的來源只有 `config`（`TAIL_HISTORY_LINES`／
/// `TAIL_MAX_BYTES`／`TAIL_TIMEOUT`），這個型別只是把它們一起搬運，**不提供
/// 預設**——預設散在兩處就會有兩份界。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TailBounds {
    /// 向後要求的 **history 行數**（`-S -<n>`）。實際取得約 `n + pane 高度`
    /// ——可見區一律在內。這不是總量上限，總量看 `max_bytes`
    pub history_lines: usize,
    pub max_bytes: usize,
    pub timeout: Duration,
}

/// 一次尾行預覽的結果。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TailCapture {
    /// 已經在 byte 界內的畫面文字
    pub text: String,
    /// 是否因為 byte 界而截斷（畫面要據此標記，否則人會以為那就是全部）
    pub truncated: bool,
}

/// `capture-pane` 的 argv（純函式，測試直接驗**送出去的參數形狀**）。
///
/// `-p` 印到 stdout、`-J` 併接被 tmux 折過的行、`-S -<n>` **向後要求 n 行
/// history**。
///
/// ⚠️ `-S -<n>` 的語意不是「總共只回 n 行」：tmux 回的是「n 行 history ＋整個
/// 可見區」，實測 80x24 的 pane 印 500 行後 `-S -200` 回 224 行。這裡的 `-S`
/// 只保證**不撈整份 scrollback**；總量的硬上限是讀取迴圈的 byte 界。
pub fn tail_args(pane: &str, history_lines: usize) -> Vec<String> {
    vec![
        "capture-pane".to_string(),
        "-p".to_string(),
        "-J".to_string(),
        "-t".to_string(),
        pane.to_string(),
        "-S".to_string(),
        format!("-{history_lines}"),
    ]
}

/// 有界讀取的收場。**逾時不在這裡**：那條路徑回的是 `None`（reader 還卡著，
/// 連 outcome 都生不出來）。
///
/// 三種收場的收屍與判定各不相同（跨廠複核 M3）：只有 `Eof` 那條可以、也必須
/// 去看子行程的真實退出碼；`Capped` 是**本方**主動停讀，非零退出多半正是我們
/// 殺出來的，拿它判失敗會把一次合法的截斷變成 unavailable。
#[derive(Debug)]
enum ReadOutcome {
    /// 來源自己結束（EOF）
    Eof(Vec<u8>),
    /// 撞到 byte 界，本方主動停讀
    Capped(Vec<u8>),
    /// 讀取途中出錯
    ReadError,
}

/// **有界讀取**：分塊讀到 `max_bytes` 為止，或到 `deadline` 為止。
///
/// 回 `None`＝逾時或 reader thread 起不來（呼叫端負責殺子行程）。
///
/// 為什麼要一條 reader thread：`Read` 沒有 deadline 的概念，唯一能在「讀取
/// 卡住」時仍然收手的辦法就是把讀取放到另一條 thread、主緒等一個有逾時的
/// channel。**不 join**：卡住的那條 thread 會在子行程被殺、pipe EOF 之後自己
/// 收斂；join 它等於把逾時又還回去。
///
/// 每一輪只遞 `min(剩餘額度 + 1, chunk)` 的 slice（跨廠複核 m1）：
/// - 來源實際被 consume 的量最多超出上限 **1 byte**（用整個 chunk 去讀，剩餘
///   額度小於 chunk 時會白白吞掉後面的資料）
/// - `truncated` 只有在**真的看到多出來那一 byte** 時才設。恰好等於上限的來源
///   是「完整讀完」，標成截斷會讓畫面說一件不存在的事
fn read_capped<R: std::io::Read + Send + 'static>(
    reader: R,
    max_bytes: usize,
    deadline: Instant,
) -> Option<ReadOutcome> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("ab-tail-read".to_string())
        .spawn(move || {
            let mut reader = reader;
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            let outcome = loop {
                // 多要 1 byte：那一 byte 的存在**就是**「來源還有東西」的證據
                let want = (max_bytes - buf.len() + 1).min(chunk.len());
                match std::io::Read::read(&mut reader, &mut chunk[..want]) {
                    Ok(0) => break ReadOutcome::Eof(buf),
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.len() > max_bytes {
                            // 停讀＝不再把後面的東西搬進記憶體。子行程可能因此
                            // 卡在寫入，呼叫端接著會殺它
                            buf.truncate(max_bytes);
                            break ReadOutcome::Capped(buf);
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break ReadOutcome::ReadError,
                }
            };
            let _ = tx.send(outcome);
        })
        .is_ok();
    if !spawned {
        return None;
    }
    let left = deadline.saturating_duration_since(Instant::now());
    rx.recv_timeout(left).ok()
}

/// 一次尾行取得的完整結果（含收屍證據）。**測試看得到 `status`／`killed`**
/// ——「逾時之後子行程真的被殺並收屍」這件事，只驗 `capture` 是驗不到的
/// （跨廠複核 m2）。
#[derive(Debug)]
struct TailRun {
    capture: Option<TailCapture>,
    /// 收屍拿到的退出狀態（`None`＝連 `wait` 都失敗）。**只有測試讀它**——
    /// production 只要 `capture`，但把收屍證據丟掉就等於那條保證沒人驗得到
    #[cfg_attr(not(test), allow(dead_code))]
    status: Option<std::process::ExitStatus>,
    /// 本方是否主動送過 kill（同上：測試用的收屍證據）
    #[cfg_attr(not(test), allow(dead_code))]
    killed: bool,
}

/// 以**已經起好的子行程**執行有界讀取＋收屍（與「怎麼起 tmux」分開，測試才
/// 注得進可控的 fixture process——真 tmux 做不出決定性的 hang）。
///
/// 退出碼的分流是這一段的重點（跨廠複核 M3）：
/// - `Eof`：在剩餘期限內取得**真實**退出狀態，非零一律 `None`（fail-closed，
///   對齊 trait 註解）。pane 已消失時 `capture-pane` 正是「非零退出＋空 stdout」
///   ——不看退出碼就會把它報成「這個 pane 沒有輸出」
/// - `Capped`：本方刻意提前停讀，kill＋收屍後回**合法的**截斷結果。這條路徑
///   MUST NOT 檢查退出碼：那多半是自己殺出來的
/// - 逾時／`ReadError`：kill＋收屍後 `None`
fn tail_from_child(mut child: std::process::Child, max_bytes: usize, deadline: Instant) -> TailRun {
    let Some(stdout) = child.stdout.take() else {
        // `Stdio::piped()` 下不可達；真的走到就與逾時同一條收尾路徑
        return reap(child, None);
    };
    let text = |buf: Vec<u8>| TailCapture {
        // 畫面文字只用來給人看，非 payload 路徑（同 `capture_pane`）
        text: String::from_utf8_lossy(&buf).into_owned(),
        truncated: false,
    };
    match read_capped(stdout, max_bytes, deadline) {
        // 完整讀完：**不先殺**——先殺再看退出碼會把正常完成也變成失敗
        Some(ReadOutcome::Eof(buf)) => match wait_deadline(&mut child, deadline) {
            Some(status) if status.success() => TailRun {
                capture: Some(text(buf)),
                status: Some(status),
                killed: false,
            },
            // 非零退出（pane 不在、target 打錯…）＝查不出來，不是「沒有輸出」
            Some(status) => TailRun {
                capture: None,
                status: Some(status),
                killed: false,
            },
            // 讀完了卻等不到退出狀態：期限已到，殺掉收屍當查不出來
            None => reap(child, None),
        },
        Some(ReadOutcome::Capped(buf)) => {
            let mut run = reap(child, None);
            run.capture = Some(TailCapture {
                truncated: true,
                ..text(buf)
            });
            run
        }
        Some(ReadOutcome::ReadError) | None => reap(child, None),
    }
}

/// kill＋收屍（不留殭屍）。`capture` 由呼叫端決定要不要放回去。
fn reap(mut child: std::process::Child, capture: Option<TailCapture>) -> TailRun {
    let killed = child.kill().is_ok();
    TailRun {
        capture,
        status: child.wait().ok(),
        killed,
    }
}

/// 在期限內等子行程結束。逾時回 `None`（**不殺**——殺與否由呼叫端依收場決定）。
///
/// 輪詢間隔沿用 `wait_with_timeout` 的 20ms：EOF 之後子行程通常已經在結束途中，
/// 這裡只是不讓「stdout 關了卻不退出」的病態情況把 UI 的一次預覽拖成無限等待。
fn wait_deadline(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            // EINTR：輪詢被訊號打斷，子行程沒事（同 `wait_with_timeout`）
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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

    fn capture_pane_tail(&self, pane: &str, bounds: TailBounds) -> Option<TailCapture> {
        // **不走 `run_bounded`**：那條的 stdout 是 `read_to_end`（byte 無界），
        // 逾時又吃 `AGENT_BRIDGE_TMUX_TIMEOUT=0` 的無限逃生口。它是 spawn／
        // notify 全線在用的路徑，語意不動；這裡自己起子行程
        let deadline = Instant::now().checked_add(bounds.timeout)?;
        let args = tail_args(pane, bounds.history_lines);
        let child = Command::new("tmux")
            .args(args.iter().map(String::as_str))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // stderr 丟掉：這條路徑的失敗訊號是**退出碼**，錯誤原文沒有消費者
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        tail_from_child(child, bounds.max_bytes, deadline).capture
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
/// （`page::SubprocessRunner` 共用同一份：自訂 notifier 也是不可信的外部行程，
/// 「等不到就殺、不留殭屍」的推理逐字適用，另寫一份只會讓 EINTR 與溢位那兩個
/// 教訓少一邊。）
pub(crate) fn wait_with_timeout(
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

    // ---- P4.7 切片 D：尾行預覽的三重界 ----

    /// 驗的是**送出去的參數形狀**，不是 tmux 的回傳語意。
    ///
    /// ⚠️ 這條測試在切片 D 的第一版曾經把一個**錯誤的語意**驗成綠燈：當時的
    /// 名字與註解都寫著「總共只取 n 行」，而 `-S -<n>` 實際上是「n 行 history
    /// ＋整個可見區」（真 tmux 實測 200 → 224 行）。argv 測試證明不了外部工具
    /// 的回傳語意——那一半由分組 45 的真 tmux 測試負責（G2）。
    #[test]
    fn the_tail_asks_tmux_for_a_bounded_span_of_history() {
        let args = tail_args("%7", 200);
        assert_eq!(
            args,
            vec!["capture-pane", "-p", "-J", "-t", "%7", "-S", "-200"],
            "參數形狀：-p 印 stdout、-J 併軟折行、-S -<n> 向後要 n 行 history"
        );
        // history 行數是參數，不是寫死的
        assert_eq!(tail_args("%7", 5).last().unwrap(), "-5");
    }

    /// 記下**實際被 consume 的 byte 數**的包裝（跨廠複核 m1：只驗 buffer 長度
    /// 的話，「每次都遞完整 chunk、白白吞掉後面的資料」是驗不出來的）。
    struct Counting<R> {
        inner: R,
        n: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl<R: std::io::Read> std::io::Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let got = self.inner.read(buf)?;
            self.n.fetch_add(got, std::sync::atomic::Ordering::SeqCst);
            Ok(got)
        }
    }

    /// **byte 界成立於讀取迴圈**（gate (d)）：來源是「無限長的單行」，讀進來的
    /// bytes MUST 不超過上限——證明不是先全讀再截。
    ///
    /// 而且**最多只多 consume 1 byte**：那一 byte 正是「來源還有東西」的證據，
    /// 也是 `truncated` 唯一的依據。
    #[test]
    fn the_tail_stops_reading_at_the_byte_bound() {
        let deadline = Instant::now() + Duration::from_secs(5);
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // `io::repeat` 是永遠讀得到東西的來源：`read_to_end` 對它是不會結束的
        let src = Counting {
            inner: std::io::repeat(b'x'),
            n: n.clone(),
        };
        let Some(ReadOutcome::Capped(buf)) = read_capped(src, 4096, deadline) else {
            panic!("無限來源 MUST 收在 Capped");
        };
        assert_eq!(buf.len(), 4096, "交出去的 MUST 剛好是上限");
        assert_eq!(
            n.load(std::sync::atomic::Ordering::SeqCst),
            4097,
            "來源實際被 consume 的量 MUST 只多那一 byte（不是整個 8192 chunk）"
        );
    }

    /// **恰好等於上限不是截斷**（跨廠複核 m1）：`truncated` 說的是「後面還有
    /// 東西沒給你」，剛好讀完卻標成截斷，等於畫面說了一件不存在的事。
    #[test]
    fn a_source_that_exactly_fills_the_bound_is_not_truncated() {
        let deadline = Instant::now() + Duration::from_secs(5);
        for (len, label) in [(4095usize, "少一 byte"), (4096, "恰好等於上限")] {
            let src = std::io::Cursor::new(vec![b'x'; len]);
            let Some(ReadOutcome::Eof(buf)) = read_capped(src, 4096, deadline) else {
                panic!("{label} MUST 收在 Eof");
            };
            assert_eq!(buf.len(), len, "{label}：MUST 完整讀完");
        }
        // 對照組：多一 byte 才算截斷
        let src = std::io::Cursor::new(vec![b'x'; 4097]);
        assert!(
            matches!(
                read_capped(src, 4096, deadline),
                Some(ReadOutcome::Capped(_))
            ),
            "多一 byte MUST 收在 Capped"
        );
    }

    /// **時間界自持**（gate (d)：hanging tmux）：讀不到東西也不會卡住——
    /// 期限到就回 `None`，呼叫端據此殺子行程並顯示逾時。
    ///
    /// 卡住的來源是**可關閉**的（跨廠複核 m2）：睡一小時的 detached thread 每
    /// 跑一次就留一條到 process 結束，測試自己不該產生那種殘留。這裡逾時之後
    /// 主動放行，並等 reader 真的收斂。
    #[test]
    fn the_tail_gives_up_on_a_hanging_source() {
        /// 阻塞到測試放行為止的來源（放行＝EOF）。`Drop` 時回報，證明 reader
        /// thread 真的收斂了、不是留在那裡睡
        struct Hanging {
            gate: std::sync::mpsc::Receiver<()>,
            done: std::sync::mpsc::Sender<()>,
        }
        impl std::io::Read for Hanging {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                let _ = self.gate.recv(); // 發送端 drop → 立刻返回 → EOF
                Ok(0)
            }
        }
        impl Drop for Hanging {
            fn drop(&mut self) {
                let _ = self.done.send(());
            }
        }
        let (release, gate) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let src = Hanging {
            gate,
            done: done_tx,
        };

        let started = Instant::now();
        let deadline = Instant::now() + Duration::from_millis(150);
        assert!(
            read_capped(src, 4096, deadline).is_none(),
            "卡住的來源 MUST 逾時，不得無限等"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "而且要在期限的量級內回來（實測 {:?}）",
            started.elapsed()
        );

        // 放行：真實情境裡這一步是「子行程被殺、pipe EOF」
        drop(release);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reader thread MUST 在來源關閉後收斂（不留 detached thread）");
    }

    /// 收屍是**這條路徑自己的責任**（跨廠複核 m2）：卡住的子行程逾時之後 MUST
    /// 被殺並 `wait` 到底，否則每按一次 `L` 就留一個殭屍。
    ///
    /// 用可控的 fixture process 而不是真 tmux：真 tmux 做不出決定性的 hang。
    #[test]
    fn a_hanging_child_is_killed_and_reaped_on_timeout() {
        // 開著 stdout 卻永不寫入也永不結束：正是「卡住的 tmux」的形狀
        let child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep 應可 spawn");
        let started = Instant::now();
        let run = tail_from_child(child, 4096, Instant::now() + Duration::from_millis(200));
        assert!(run.capture.is_none(), "逾時 MUST 回 None，不是空 capture");
        assert!(run.killed, "MUST 主動送 kill");
        let status = run.status.expect("MUST 收屍（wait 到底），不留殭屍");
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(9), "終結方式 MUST 是本方的 SIGKILL");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "MUST 在期限的量級內回來（實測 {:?}）",
            started.elapsed()
        );
    }

    /// **非零退出 ≠ 沒有輸出**（跨廠複核 M3）：pane 已消失時 `capture-pane` 是
    /// 「非零退出＋空 stdout」——不看退出碼就會把它畫成「這個 pane 沒有輸出」，
    /// 與 trait 註解承諾的 fail-closed 直接矛盾。
    #[test]
    fn a_child_that_exits_non_zero_is_unavailable_not_empty() {
        let child = Command::new("sh")
            .args(["-c", "exit 3"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sh 應可 spawn");
        let run = tail_from_child(child, 4096, Instant::now() + Duration::from_secs(5));
        assert!(run.capture.is_none(), "非零退出 MUST 回 None");
        assert!(
            !run.killed,
            "正常結束的子行程 MUST NOT 被殺（先殺再看退出碼會把成功也變成失敗）"
        );
        assert_eq!(
            run.status.and_then(|s| s.code()),
            Some(3),
            "MUST 是真實退出碼"
        );
    }

    /// 正常路徑：完整讀完＋退出碼 0 → 有內容、未截斷、**沒有被殺**。
    #[test]
    fn a_child_that_succeeds_yields_its_output_untruncated() {
        let child = Command::new("sh")
            .args(["-c", "printf 'a\\nb\\n'"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sh 應可 spawn");
        let run = tail_from_child(child, 4096, Instant::now() + Duration::from_secs(5));
        let cap = run.capture.expect("成功路徑 MUST 有內容");
        assert_eq!(cap.text, "a\nb\n");
        assert!(!cap.truncated);
        assert!(!run.killed, "成功路徑 MUST NOT 送 kill");
        assert!(run.status.is_some_and(|s| s.success()));
    }

    /// 截斷路徑：本方主動停讀 → kill＋收屍，但結果**仍然合法**。
    ///
    /// 這條釘的是 M3 修法裡最容易做錯的一格：`Capped` 之後去檢查退出碼，等於
    /// 拿自己殺出來的非零狀態否定一次合法的截斷。
    #[test]
    fn a_capped_child_still_yields_a_truncated_capture() {
        let child = Command::new("sh")
            // 一直吐字元、不會自己結束：撞 byte 界的形狀
            .args(["-c", "while :; do printf 'xxxxxxxxxxxxxxxx'; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sh 應可 spawn");
        let run = tail_from_child(child, 4096, Instant::now() + Duration::from_secs(5));
        let cap = run.capture.expect("截斷 MUST 仍有內容（不是 unavailable）");
        assert_eq!(cap.text.len(), 4096);
        assert!(cap.truncated, "截斷 MUST 說出來");
        assert!(run.killed, "停讀之後 MUST 殺掉還在寫的子行程");
        assert!(run.status.is_some(), "MUST 收屍");
    }

    /// 三個界的值只有一份定義（`config`），這裡釘住它們是**有限**的正數
    /// ——常數被改成 0／`usize::MAX` 時，上面三條測試都還會綠。
    #[test]
    fn the_tail_bounds_are_finite() {
        // 常數 → `const` 區塊裡斷言（clippy 的建議，順帶把它變成編譯期關卡：
        // 把界設成 0／無限大的那一版根本編不過）
        const {
            assert!(config::TAIL_HISTORY_LINES > 0 && config::TAIL_HISTORY_LINES <= 10_000);
        }
        const {
            assert!(config::TAIL_MAX_BYTES >= 1024 && config::TAIL_MAX_BYTES <= 1 << 20);
        }
        assert!(
            config::TAIL_TIMEOUT >= Duration::from_millis(200)
                && config::TAIL_TIMEOUT <= Duration::from_secs(30)
        );
    }
}
