//! `ab` — agent-bridge CLI binary。argv 手寫解析＋dispatch＋stderr/exit code
//! （架構 §1／§4）。M2 範圍：register/unregister/list/send/receive/start/
//! reply/fail/cancel/status/read/await/gc/hook。spawn 生命週期群屬 M3，
//! 未實作前一律走「未知指令」分支（不虛構半成品行為）。

use std::process::ExitCode;

use ab_core::config;
use ab_core::error::{Error, Result};
use ab_core::hook::{self, HookOutcome};
use ab_core::lock::acquire_lock;
use ab_core::notify;
use ab_core::paths::Paths;
use ab_core::registry;
use ab_core::task::{self, MessageSource, TaskState};
use ab_core::tmux::SubprocessTmux;
use ab_core::validate::is_valid_name;

// 逐字對齊 bash `usage()`:84（bin/agent-bridge）的 heredoc 內容。
const USAGE: &str = r#"用法：
  agent-bridge register <agent> <tmux-target>   註冊 agent 與其 tmux pane
  agent-bridge unregister <agent>               移除 agent 註冊
  agent-bridge list                             列出已註冊 agent（name<TAB>pane_id<TAB>ready）
  agent-bridge send <agent> --from <sender> (--message <text> | --message-file <path>)
                                                委派任務；stdout 只印 task-id
  agent-bridge receive <task-id>                取出任務（標頭走 stderr、request 原文走 stdout）
  agent-bridge start <task-id>                  （worker）標記開工（delivered → running）
  agent-bridge reply <task-id> (--message <text> | --message-file <path>)
                                                回覆任務（delivered/running 可回）
  agent-bridge fail <task-id> (--message <text> | --message-file <path>)
                                                回報任務失敗（delivered/running 可；訊息＝失敗原因）
  agent-bridge cancel <task-id>                 取消任務（queued/delivered/running 可）
  agent-bridge status <task-id>                 印裸狀態字（queued/delivered/running/completed/failed/cancelled）
  agent-bridge read <task-id>                   讀回覆（completed/failed 可；標頭走 stderr、response 原文走 stdout）
  agent-bridge await <task-id> [--timeout <secs>]
                                                阻塞等待 task 到終態，印裸狀態字後 exit 0；
                                                逾時以 exit 124 退出（其他錯誤一律非 124，供呼叫端區分）
  agent-bridge spawn <name> --runtime <codex|claude> [--model <model>] [--window]
                                                spawn 一個 worker pane 並註冊；stdout 只印 pane-id
                                                （--model 不給＝該 CLI 的使用者預設模型）
  agent-bridge relay <name> --runtime <codex|claude> [--model <model>] --handoff <path> [--window] [--no-select] [--self-exit <my-name>]
                                                把主導權交給新 session（注入接手者守則＋交接檔）；stdout 只印 pane-id
  agent-bridge despawn <name>                   回收 spawn 出身的 worker（kill pane＋除名；人工註冊拒殺）
  agent-bridge ready <name>                     （worker）回報就緒；僅限 spawned agent
  agent-bridge disposable <name>                （worker）宣告本輪脈絡已無殘值，可即時回收
                                                （預設是保留：沒宣告過的一律視為仍有殘值）
  agent-bridge idle                             worker 池回收決策視圖；唯讀
                                                （name<TAB>ready<TAB>disposable<TAB>idle_secs）
  agent-bridge evict <name> [--timeout <secs>] [--from <sender>]
                                                驅逐 worker：派收尾任務 → 等筆記落地 → despawn
                                                stdout 只印收尾 task-id；逾時仍 despawn（審計記 evicted-timeout）
  agent-bridge gc [--older-than <days>] [--apply] [--include-notes]
                                                清掉夠舊的終態 task（預設 14 天）；未完成的與
                                                evict 收尾筆記一律保留。預設只試算，--apply 才刪
  agent-bridge hook <stop|prompt-submit|notification>
                                                （由 Claude Code 呼叫，非人工手動用）落地 hook
                                                協定：stdin 收事件 JSON，依 AGENT_BRIDGE_SPAWN_TAG
                                                析出的 agent 名更新 state/<name>.json；stop 事件
                                                查到 mailbox 有新任務時輸出 block JSON 續跑，
                                                stdout 只在此時有內容。任何內部錯誤一律 exit 0。

--message-file 用 `-` 表示讀 stdin。
資料目錄預設 ~/.local/share/agent-bridge/，可用 AGENT_BRIDGE_DATA 覆蓋。
await 輪詢間隔預設 1 秒，可用 AGENT_BRIDGE_POLL_INTERVAL 覆蓋；--timeout 0（預設）＝不逾時。
evict 的 --timeout 預設 300 秒、--from 預設 orchestrator；--timeout 0＝無限等，
等於放棄「一定騰得出 cap」這個保證，只在確定 worker 還活著時才用。
spawn 上限 AGENT_BRIDGE_MAX_SPAWN（預設 4）；就緒等待 AGENT_BRIDGE_READY_TIMEOUT
秒（預設 30，0＝不等待），探針重送間隔 AGENT_BRIDGE_READY_PROBE_INTERVAL（預設 2）。
worker 活動狀態新鮮度 AGENT_BRIDGE_STATE_TTL（預設 1800 秒）：send/reply/cancel
通知前若對方 state 檔顯示 busy 且未超過這個秒數，改為完全不送鍵（改由對方
Stop hook 在 turn 結束時自行 receive）；state 檔缺失或已過期一律退回既有的
tmux send-keys 通知路徑。
claude worker 的 hooks settings 預設 share/claude-worker-hooks.json，可用
AGENT_BRIDGE_CLAUDE_HOOKS 覆蓋；停用 hooks 就指向一份 hooks 為空的合法 JSON。
"#;

fn print_usage() {
    eprint!("{USAGE}");
}

fn err_line(msg: &str) {
    eprintln!("agent-bridge: {msg}");
}

// 隱藏內省（非 spec 契約面）：一行一個「已完整實作」的契約子指令。
// 用途：里程碑 gate 的 capability 核對，堵「純 assert_fails 分組靠
// unknown-command fallback 假綠」。M2 起 hook 就位；spawn 生命週期群
// （spawn/relay/despawn/ready/disposable/idle/evict）屬 M3，
// 未實作前不列——列入等於宣稱可用。
fn print_implemented_commands() {
    for cmd in [
        "await",
        "cancel",
        "fail",
        "gc",
        "hook",
        "list",
        "read",
        "receive",
        "register",
        "reply",
        "send",
        "start",
        "status",
        "unregister",
    ] {
        println!("{cmd}");
    }
}

/// 架構 §5：Rust 預設把 SIGPIPE 設成 `SIG_IGN`，寫端因此拿到 `EPIPE` 錯誤而
/// 不是隨管線死。bash 正本的行為是後者——`ab read <id> | head -1` 這類用法下，
/// 讀端關閉時整個行程應以 SIGPIPE 收場（呼叫端看到 141），而不是印一行寫入
/// 失敗再非零退出。
///
/// std 沒有 signal API，為此引入 `libc`（零傳遞依賴）。另一條路是在每個
/// stdout 寫出點攔 `EPIPE` 再自行 `exit(141)`：碼更多、每新增一個輸出點就多
/// 一個漏接的機會，而且被訊號殺死與自行退出對呼叫端的 `WIFSIGNALED` 仍不等
/// 價。這一行換掉那一整類問題。
///
/// **hook 不套用這條**：`hook` 的鐵律是任何情況都 exit 0，而 SIG_DFL 之下
/// stdout 是已關閉管線時整個行程會被訊號殺死（141）。bash 那邊 `jq … || true`
/// 把寫出失敗吞掉、照樣 `exit 0`，所以 hook 分支維持 Rust 預設的忽略
/// （codex 複核 2026-07-31 finding 1）。
///
/// **驗證缺口（誠實記錄）**：測試套件目前沒有任何一組斷言 SIGPIPE 行為
/// （分組 8 是「通知失敗路徑」，與此無關），所以這條沒有機器 gate 護著。
fn restore_sigpipe_default() {
    // SAFETY: `signal(2)` 在單執行緒的行程起手處呼叫；SIG_DFL 是恢復預設
    // 處置，不涉及自訂 handler 的可重入性問題。
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// hook 的 block JSON 寫出：**不能用 `println!`**——它遇寫入失敗會 panic
/// （stdout 指向 `/dev/full`、或已關閉的管線），退出碼變 101，違反 exit 0
/// 鐵律。bash 那邊是 `jq … 2>/dev/null || true`：寫不出去就算了，照樣 exit 0
/// （codex 複核 2026-07-31 finding 1）。
fn write_stdout_lossy(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn main() -> ExitCode {
    // argv 走 `args_os`：`std::env::args()` 對非 UTF-8 參數是 **panic**，而
    // hook 的鐵律不允許任何非零退出（`ab hook <0xff>` 在 bash 是 rc=0，改前
    // 的 Rust 是 101——panic 發生在 catch_unwind 之外）。指令名本身無法轉成
    // UTF-8 時視為未知指令，與 bash 的 `*) die "未知指令"` 同向。
    //
    // 遺留缺口（誠實記錄）：非指令名的參數在此走 lossy 轉換，`--message` 帶
    // 非 UTF-8 位元組時內容會被換成 U+FFFD，bash 則原樣保真。要真正對齊得把
    // 訊息路徑整條改走 `OsStr`/bytes，屬 M3 的 argv 面工作；至少不再 panic。
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if raw.is_empty() {
        print_usage();
        return ExitCode::from(1);
    }
    let cmd = raw[0].to_str().unwrap_or("").to_string();
    let args: Vec<String> = raw
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();

    // hook 最先分流，且**在恢復 SIGPIPE 之前**：見 restore_sigpipe_default。
    if cmd == "hook" {
        let paths = Paths::resolve();
        // 唯讀豁免（CLI-RO-1）：hook 不建資料目錄
        //
        // 兜底放在 dispatch 而不是 hook 模組內，是為了讓「exit 0」在這一眼就
        // 看得到，不必追進去確認。panic 也一併吃掉：預設 hook 仍會把訊息印上
        // stderr（同 bash 出錯時也會有輸出），但退出碼不得因此變成 101。
        let rest = &args[1..];
        let outcome =
            std::panic::catch_unwind(|| hook::run(&paths, rest)).unwrap_or(HookOutcome::Silent);
        if let HookOutcome::Block(doc) = outcome {
            write_stdout_lossy(&doc);
        }
        return ExitCode::from(0);
    }

    restore_sigpipe_default();
    if matches!(cmd.as_str(), "-h" | "--help" | "help") {
        print_usage();
        return ExitCode::from(0);
    }
    if cmd == "__implemented-commands" {
        print_implemented_commands();
        return ExitCode::from(0);
    }
    let rest = &args[1..];

    let paths = Paths::resolve();
    // 唯讀豁免（main dispatch:2262-2269／CLI-RO-1）：status/await/idle/list/
    // hook 不建目錄。本切片只實作 list 屬於這個豁免表；其餘四個尚未實作，
    // 一律落入下面的「未知指令」分支（不建目錄與否對它們無意義）。
    let readonly = matches!(cmd.as_str(), "status" | "await" | "idle" | "list");
    if !readonly && let Err(e) = paths.ensure_dirs() {
        err_line(&e.message);
        return ExitCode::from(1);
    }

    let result = match cmd.as_str() {
        "register" => cmd_register(&paths, rest),
        "unregister" => cmd_unregister(&paths, rest),
        "list" => cmd_list(&paths, rest),
        "send" => cmd_send(&paths, rest),
        "receive" => cmd_receive(&paths, rest),
        "start" => cmd_start(&paths, rest),
        "reply" => cmd_reply(&paths, rest),
        "fail" => cmd_fail(&paths, rest),
        "cancel" => cmd_cancel(&paths, rest),
        "status" => cmd_status(&paths, rest),
        "read" => cmd_read(&paths, rest),
        "await" => cmd_await(&paths, rest),
        "gc" => cmd_gc(&paths, rest),
        _ => {
            print_usage();
            Err(Error::new(format!("未知指令：{cmd}")))
        }
    };

    match result {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            err_line(&e.message);
            ExitCode::from(1)
        }
    }
}

/// cmd_register:476
fn cmd_register(paths: &Paths, args: &[String]) -> Result<()> {
    if args.len() != 2 {
        return Err(Error::new(
            "用法：agent-bridge register <agent> <tmux-target>",
        ));
    }
    let name = &args[0];
    let target = &args[1];
    let tmux = SubprocessTmux;
    let pane = registry::register(paths, &tmux, name, target)?;
    eprintln!("已註冊 agent '{name}' → pane {pane}");
    Ok(())
}

/// cmd_list:522 — 無參數檢查，忽略多餘參數（比照 bash 未檢查 $#）。
/// 損壞 registry 檔 MUST NOT 靜默略過（見 registry::list 文件）：`?` 把
/// `Err` 上拋給 main() 的統一收斂層，印訊息＋非零退出。
fn cmd_list(paths: &Paths, _args: &[String]) -> Result<()> {
    for (name, pane, ready) in registry::list(paths)? {
        println!("{name}\t{pane}\t{ready}");
    }
    Ok(())
}

/// cmd_send:536 — 本切片只涵蓋分組 2「send 錯誤路徑」：引數解析、
/// `--from`/`--message`|`--message-file` 擇一驗證、名稱文法、收件者已註冊
/// 檢查，逐項對齊 CLI-SEND-2/3 與 STATE-GEN-3。task 建立成功路徑（寫
/// request/metadata/status、通知）留給下一階段，見 ab-core::registry
/// 與架構文件的 task/notify 模組對映——尚未實作，命中時明確回錯而非
/// 產生半成品任務。
fn cmd_send(paths: &Paths, args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err(Error::new(
            "用法：agent-bridge send <agent> --from <sender> (--message <text> | --message-file <path>)",
        ));
    }
    let to = args[0].clone();
    let mut it = args[1..].iter();
    let mut from = String::new();
    let mut mode = String::new(); // "" | "text" | "file"
    let mut val = String::new();

    while let Some(a) = it.next() {
        match a.as_str() {
            "--from" => {
                let v = it.next().ok_or_else(|| Error::new("--from 需要參數"))?;
                from = v.clone();
            }
            "--message" => {
                let v = it.next().ok_or_else(|| Error::new("--message 需要參數"))?;
                if !mode.is_empty() {
                    return Err(Error::new("--message 與 --message-file 只能擇一"));
                }
                mode = "text".to_string();
                val = v.clone();
            }
            "--message-file" => {
                let v = it
                    .next()
                    .ok_or_else(|| Error::new("--message-file 需要參數"))?;
                if !mode.is_empty() {
                    return Err(Error::new("--message 與 --message-file 只能擇一"));
                }
                mode = "file".to_string();
                val = v.clone();
            }
            other => return Err(Error::new(format!("未知參數：{other}"))),
        }
    }

    if from.is_empty() {
        return Err(Error::new("send 需要 --from <sender>"));
    }
    if mode.is_empty() {
        return Err(Error::new("send 需要 --message 或 --message-file"));
    }
    if mode == "file" && val != "-" && !std::path::Path::new(&val).is_file() {
        return Err(Error::new(format!("找不到訊息檔：{val}")));
    }
    if !is_valid_name(&to) {
        return Err(Error::new(format!(
            "agent 名稱不合法（僅允許 [A-Za-z0-9_-]+）：{to}"
        )));
    }
    if !is_valid_name(&from) {
        return Err(Error::new(format!(
            "sender 名稱不合法（僅允許 [A-Za-z0-9_-]+）：{from}"
        )));
    }
    let agent_file = paths.agents_dir.join(format!("{to}.json"));
    if !agent_file.is_file() {
        return Err(Error::new(format!(
            "未註冊的 agent：{to}（先用 agent-bridge register）"
        )));
    }
    if registry::is_spawned_not_ready(&agent_file) {
        err_line(&format!(
            "警告：agent '{to}' 尚未回報就緒（starting），通知可能延後；訊息已入 mailbox 不會遺失"
        ));
    }

    let src = message_source(&mode, &val);
    let task_id = task::create_task(paths, &from, &to, &src, false)?;

    // 通知前重讀 pane（cmd_send:613-619）：從參數檢查到這裡隔著建目錄＋三次
    // 寫檔，期間同名 agent 可能被 unregister＋register 換到別的 pane——舊 pane
    // 若已屬別人的 session，這行 command＋Enter 就打進無辜視窗。重讀把窗口縮到
    // 次毫秒級；徹底關閉需要「讀 registry 與 send-keys」原子化，tmux 給不了。
    let pane = registry::read_pane(&agent_file);
    let tmux = SubprocessTmux;
    notify::notify_or_defer(
        paths,
        &tmux,
        &to,
        &pane,
        &format!("agent-bridge receive {task_id}"),
        &task_id,
        "receive",
    )?;

    println!("{task_id}");
    Ok(())
}

/// write_message 的 mode/val 二元組 → `MessageSource`（`--message-file -`＝stdin）。
fn message_source(mode: &str, val: &str) -> MessageSource {
    if mode == "text" {
        MessageSource::Text(val.to_string())
    } else if val == "-" {
        MessageSource::Stdin
    } else {
        MessageSource::File(std::path::PathBuf::from(val))
    }
}

/// parse_message_opts:664 — reply／fail 共用的 `--message`／`--message-file`
/// 解析。`cmdname` 只用在「需要訊息」那句 die 的措辭裡，逐字對齊 bash。
fn parse_message_opts(cmdname: &str, args: &[String]) -> Result<MessageSource> {
    let mut mode = String::new();
    let mut val = String::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--message" => {
                let v = it.next().ok_or_else(|| Error::new("--message 需要參數"))?;
                if !mode.is_empty() {
                    return Err(Error::new("--message 與 --message-file 只能擇一"));
                }
                mode = "text".to_string();
                val = v.clone();
            }
            "--message-file" => {
                let v = it
                    .next()
                    .ok_or_else(|| Error::new("--message-file 需要參數"))?;
                if !mode.is_empty() {
                    return Err(Error::new("--message 與 --message-file 只能擇一"));
                }
                mode = "file".to_string();
                val = v.clone();
            }
            other => return Err(Error::new(format!("未知參數：{other}"))),
        }
    }
    if mode.is_empty() {
        return Err(Error::new(format!(
            "{cmdname} 需要 --message 或 --message-file"
        )));
    }
    Ok(message_source(&mode, &val))
}

/// cmd_unregister:503
fn cmd_unregister(paths: &Paths, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(Error::new("用法：agent-bridge unregister <agent>"));
    }
    registry::unregister(paths, &args[0])?;
    eprintln!("已移除 agent '{}' 的註冊", args[0]);
    Ok(())
}

/// cmd_receive:626 — queued→delivered；delivered/running 可重複取件（re-receive）。
/// 標頭走 stderr、request 原文走 stdout（byte 原樣，CLI-RECEIVE-1）。
fn cmd_receive(paths: &Paths, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(Error::new("用法：agent-bridge receive <task-id>"));
    }
    let id = &args[0];
    task::check_task_id(id)?;
    let dir = task::require_task_dir(paths, id)?;

    let guard = acquire_lock(paths, id)?;
    let outcome = (|| -> Result<()> {
        match task::read_status(&dir)?.as_str() {
            "queued" => {
                task::update_meta_status(&dir, TaskState::Delivered)?;
                task::log_event(paths, id, "delivered", "")
            }
            "delivered" | "running" => task::log_event(paths, id, "re-receive", ""),
            st => Err(Error::new(format!(
                "task 狀態為 {st}，無法 receive（僅 queued/delivered/running 可）"
            ))),
        }
    })();
    guard.release();
    outcome?;

    eprintln!("task-id: {id}");
    eprintln!("from: {}", task::meta_str(&dir, "from")?);
    eprintln!(
        "working_directory: {}",
        task::meta_str(&dir, "working_directory")?
    );
    write_payload(&dir.join("request.md"))
}

/// cmd_start:726 — 僅 delivered 可開工。
fn cmd_start(paths: &Paths, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(Error::new("用法：agent-bridge start <task-id>"));
    }
    let id = &args[0];
    task::check_task_id(id)?;
    let dir = task::require_task_dir(paths, id)?;

    let guard = acquire_lock(paths, id)?;
    let outcome = (|| -> Result<()> {
        let st = task::read_status(&dir)?;
        if st != "delivered" {
            return Err(Error::new(format!(
                "task 狀態為 {st}，僅 delivered 可 start（queued 請先 receive）"
            )));
        }
        task::update_meta_status(&dir, TaskState::Running)?;
        task::log_event(paths, id, "started", "")
    })();
    guard.release();
    outcome?;
    eprintln!("task {id} 開工（running）");
    Ok(())
}

/// respond_task:686 — reply／fail 的共用主體：寫 response.md、轉終態、
/// 通知 sender 讀回覆。
fn respond_task(
    paths: &Paths,
    id: &str,
    final_state: TaskState,
    event: &str,
    cmdname: &str,
    src: &MessageSource,
) -> Result<()> {
    let dir = task::require_task_dir(paths, id)?;

    let guard = acquire_lock(paths, id)?;
    let outcome = (|| -> Result<()> {
        let st = task::read_status(&dir)?;
        if st != "delivered" && st != "running" {
            return Err(Error::new(format!(
                "task 狀態為 {st}，僅 delivered/running 可 {cmdname}（queued 請先 receive；終態不可再變更）"
            )));
        }
        task::write_message(&dir.join("response.md"), src)?;
        task::update_meta_status(&dir, final_state)?;
        task::log_event(paths, id, event, "")
    })();
    guard.release();
    outcome?;

    // 通知 sender 來讀回覆；sender 未註冊（或已除名）就只是不通知，不是錯誤。
    let from = task::meta_str(&dir, "from")?;
    let agent_file = paths.agents_dir.join(format!("{from}.json"));
    if agent_file.is_file() {
        let pane = registry::read_pane(&agent_file);
        let tmux = SubprocessTmux;
        notify::notify_or_defer(
            paths,
            &tmux,
            &from,
            &pane,
            &format!("agent-bridge read {id}"),
            id,
            "read",
        )?;
    }
    Ok(())
}

/// cmd_reply:708
fn cmd_reply(paths: &Paths, args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err(Error::new(
            "用法：agent-bridge reply <task-id> (--message <text> | --message-file <path>)",
        ));
    }
    let id = &args[0];
    task::check_task_id(id)?;
    let src = parse_message_opts("reply", &args[1..])?;
    respond_task(paths, id, TaskState::Completed, "replied", "reply", &src)?;
    eprintln!("已回覆 task {id}（completed）");
    Ok(())
}

/// cmd_fail:717
fn cmd_fail(paths: &Paths, args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err(Error::new(
            "用法：agent-bridge fail <task-id> (--message <text> | --message-file <path>)",
        ));
    }
    let id = &args[0];
    task::check_task_id(id)?;
    let src = parse_message_opts("fail", &args[1..])?;
    respond_task(paths, id, TaskState::Failed, "failed", "fail", &src)?;
    eprintln!("已回報 task {id} 失敗（failed）");
    Ok(())
}

/// cmd_cancel:744 — queued/delivered/running 皆可取消；通知失敗不影響取消本身。
fn cmd_cancel(paths: &Paths, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(Error::new("用法：agent-bridge cancel <task-id>"));
    }
    let id = &args[0];
    task::check_task_id(id)?;
    let dir = task::require_task_dir(paths, id)?;

    let guard = acquire_lock(paths, id)?;
    let outcome = (|| -> Result<()> {
        let st = task::read_status(&dir)?;
        if !matches!(st.as_str(), "queued" | "delivered" | "running") {
            return Err(Error::new(format!(
                "task 狀態為 {st}，無法 cancel（僅 queued/delivered/running 可）"
            )));
        }
        task::update_meta_status(&dir, TaskState::Cancelled)?;
        task::log_event(paths, id, "cancelled", "")
    })();
    guard.release();
    outcome?;

    // 通知 worker 查狀態（會看到 cancelled）
    let to = task::meta_str(&dir, "to")?;
    let agent_file = paths.agents_dir.join(format!("{to}.json"));
    if agent_file.is_file() {
        let pane = registry::read_pane(&agent_file);
        let tmux = SubprocessTmux;
        notify::notify_or_defer(
            paths,
            &tmux,
            &to,
            &pane,
            &format!("agent-bridge status {id}"),
            id,
            "status",
        )?;
    }
    eprintln!("已取消 task {id}（cancelled）");
    Ok(())
}

/// cmd_status:770 — 印裸狀態字。唯讀（不建目錄、不取鎖、不寫 events）。
/// status 檔讀取失敗 MUST 以非零收場，不得以 rc=0＋空輸出蒙混（cmd_status:776-780）。
fn cmd_status(paths: &Paths, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(Error::new("用法：agent-bridge status <task-id>"));
    }
    let id = &args[0];
    task::check_task_id(id)?;
    let dir = task::require_task_dir(paths, id)?;
    let st = task::read_status(&dir).map_err(|_| {
        Error::new(format!(
            "task {id} 的 status 檔讀取失敗（資料損壞），請檢查 {}",
            dir.display()
        ))
    })?;
    println!("{st}");
    Ok(())
}

/// cmd_read:784 — 讀回覆（completed/failed 可）。
/// **取 task 鎖到輸出完為止**：gc --apply 刪目錄前取的就是這把鎖，不取的話
/// 從驗完狀態到讀 response 之間目錄可被整個抽走（cmd_read:791-794）。
fn cmd_read(paths: &Paths, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        return Err(Error::new("用法：agent-bridge read <task-id>"));
    }
    let id = &args[0];
    task::check_task_id(id)?;
    let dir = task::require_task_dir(paths, id)?;

    let guard = acquire_lock(paths, id)?;
    let outcome = (|| -> Result<()> {
        match task::read_status(&dir)?.as_str() {
            "completed" | "failed" => {}
            "cancelled" => {
                return Err(Error::new(format!(
                    "task {id} 已取消（cancelled），沒有回覆可讀"
                )));
            }
            st => {
                return Err(Error::new(format!(
                    "task {id} 尚未回覆（狀態：{st}）；查詢進度請用 agent-bridge status {id}"
                )));
            }
        }
        eprintln!("task-id: {id}");
        eprintln!("from: {}", task::meta_str(&dir, "from")?);
        eprintln!("to: {}", task::meta_str(&dir, "to")?);
        task::log_event(paths, id, "read", "")?;
        write_payload(&dir.join("response.md"))
    })();
    guard.release();
    outcome
}

/// cmd_await:822 — 唯讀輪詢 status 檔（不寫 events、不取鎖），在只讀 sandbox
/// 內也能用。**逾時以專屬 exit 124 退出**，其他錯誤一律非 124——呼叫端要能把
/// 「等到期限」與「await 自己壞掉」分開處置（cmd_await:866-871）。
fn cmd_await(paths: &Paths, args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err(Error::new(
            "用法：agent-bridge await <task-id> [--timeout <secs>]",
        ));
    }
    let id = &args[0];
    task::check_task_id(id)?;
    let mut timeout: u64 = 0;
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--timeout" => {
                let v = it.next().ok_or_else(|| Error::new("--timeout 需要參數"))?;
                let ok = !v.is_empty() && v.len() <= 9 && v.bytes().all(|b| b.is_ascii_digit());
                if !ok {
                    return Err(Error::new(format!(
                        "--timeout 需為非負整數（秒，至多 9 位），0＝不逾時：{v}"
                    )));
                }
                // 前導零比照 bash `10#$1` 強制十進位
                timeout = v.parse().unwrap_or(0);
            }
            other => return Err(Error::new(format!("未知參數：{other}"))),
        }
    }
    let dir = task::require_task_dir(paths, id)?;

    // 輪詢間隔在進迴圈前就驗：壞值在 bash 會讓 sleep 立刻報錯、await 毫秒級
    // 非零退出，呼叫端若把這種操作性失敗當成逾時（evict 曾如此）就會殺掉還
    // 活著的 worker。此處同樣先驗後跑，維持「124 只等於真逾時」的契約。
    // 名字取 config 的常數（集中處），解析與錯誤文案留在 CLI 層——那兩句
    // die 訊息是 await 的契約面，不是設定讀取的一部分。
    let raw = std::env::var(config::ENV_POLL_INTERVAL).unwrap_or_default();
    let interval: f64 = if raw.is_empty() {
        1.0
    } else {
        let parsed = if poll_interval_shape_ok(&raw) {
            raw.parse::<f64>().ok()
        } else {
            None
        };
        match parsed {
            Some(v) if v.is_finite() && v > 0.0 => v,
            Some(_) => {
                return Err(Error::new(format!(
                    "AGENT_BRIDGE_POLL_INTERVAL 需大於 0（否則輪詢忙迴圈）：{raw}"
                )));
            }
            None => {
                return Err(Error::new(format!(
                    "AGENT_BRIDGE_POLL_INTERVAL 需為正數（秒）：{raw}"
                )));
            }
        }
    };

    let started = std::time::Instant::now();
    loop {
        let st = task::read_status(&dir).map_err(|_| {
            Error::new(format!(
                "await 無法讀取 task {id} 的 status 檔（任務目錄被移走？）"
            ))
        })?;
        if matches!(st.as_str(), "completed" | "failed" | "cancelled") {
            println!("{st}");
            return Ok(());
        }
        if timeout > 0 && started.elapsed().as_secs() >= timeout {
            err_line(&format!(
                "await 逾時（{timeout}s）：task {id} 目前狀態 {st}"
            ));
            // 專屬退出碼：不走 main 的統一收斂層（那裡一律 1）
            std::process::exit(124);
        }
        std::thread::sleep(std::time::Duration::from_secs_f64(interval));
    }
}

/// cmd_gc:1683 — 預設只試算，`--apply` 才刪。核心在 `ab_core::task::gc`，
/// 這裡只負責參數解析與報表輸出。
fn cmd_gc(paths: &Paths, args: &[String]) -> Result<()> {
    let mut days: u64 = 14;
    let mut apply = false;
    let mut include_notes = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--older-than" => {
                let v = it
                    .next()
                    .ok_or_else(|| Error::new("--older-than 需要參數"))?;
                let ok = !v.is_empty() && v.len() <= 5 && v.bytes().all(|b| b.is_ascii_digit());
                if !ok {
                    return Err(Error::new(format!(
                        "--older-than 需為 0-99999 的整數天：{v}"
                    )));
                }
                days = v.parse().unwrap_or(14);
            }
            "--apply" => apply = true,
            "--include-notes" => include_notes = true,
            other => return Err(Error::new(format!("未知參數：{other}"))),
        }
    }

    let s = task::gc(paths, days, apply, include_notes)?;
    for (id, st, ts) in &s.candidates {
        println!("{id}\t{st}\t{ts}");
    }
    let kept = format!(
        "保留 未完成 {}／收尾筆記 {}／宣告失效證據 {}／未滿 {days} 天 {}",
        s.kept_live, s.kept_pin, s.kept_proof, s.kept_young
    );
    if apply {
        eprintln!("gc：已刪 {} 個；{kept}", s.removed);
        if s.failed > 0 {
            err_line(&format!(
                "警告：{} 個目錄未能刪除（鎖被佔用或權限問題），下次再試",
                s.failed
            ));
        }
    } else {
        eprintln!("gc（試算）：可刪 {} 個；{kept}", s.removed);
        eprintln!("確認無誤後加 --apply 才會真的刪除");
    }
    Ok(())
}

/// 精確翻譯 bash `^([0-9]+|[0-9]*\.[0-9]+)$`（bin/agent-bridge:847-848）：
/// 小數點後**至少一位數字**。`1.` 在 bash 被拒，Rust 的 `parse::<f64>` 卻
/// 收得下——不特別擋就會多接受一種 bash 判為設定錯誤的值。
fn poll_interval_shape_ok(raw: &str) -> bool {
    match raw.split_once('.') {
        None => !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()),
        Some((int_part, frac)) => {
            int_part.bytes().all(|b| b.is_ascii_digit())
                && !frac.is_empty()
                && frac.bytes().all(|b| b.is_ascii_digit())
        }
    }
}

/// payload 輸出：request/response 原文以 byte 原樣寫進 stdout，
/// 不經 `String`／lossy 轉換（分組 6 驗 byte-for-byte 保真，架構 §3）。
fn write_payload(path: &std::path::Path) -> Result<()> {
    use std::io::Write;
    let bytes =
        std::fs::read(path).map_err(|e| Error::new(format!("無法讀取 {}：{e}", path.display())))?;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(&bytes)
        .map_err(|e| Error::new(format!("無法寫入 stdout：{e}")))?;
    lock.flush()
        .map_err(|e| Error::new(format!("無法寫入 stdout：{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bash regex 的接受集合，逐點對齊（含 `1.` 這個 Rust `parse::<f64>`
    /// 會放行、bash 會拒的形狀）。
    #[test]
    fn poll_interval_shape_matches_bash_regex() {
        for good in ["1", "0", "05", "0.5", ".5", "10.25", "000"] {
            assert!(poll_interval_shape_ok(good), "應接受：{good}");
        }
        for bad in ["1.", "", ".", "1.2.3", "-1", "1e3", "abc", " 1", "1 "] {
            assert!(!poll_interval_shape_ok(bad), "應拒絕：{bad}");
        }
    }
}
