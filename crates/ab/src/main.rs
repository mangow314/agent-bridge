//! `ab` — agent-bridge CLI binary。argv 手寫解析＋dispatch＋stderr/exit code
//! （架構 §1／§4）。M0.5 spike 範圍：`register`／`list`／`send`（錯誤路徑，
//! 見 ab-core::registry 文件）；其餘子指令一律走「未知指令」分支，留給後續
//! 階段實作（不在本切片虛構半成品行為）。

use std::process::ExitCode;

use ab_core::error::Error;
use ab_core::paths::Paths;
use ab_core::registry;
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
// unknown-command fallback 假綠」。send 尚停在錯誤路徑（未建 task、未通知），
// 列入等於宣稱可用，故不列；補完快樂路徑時再加入。
fn print_implemented_commands() {
    for cmd in ["register", "list"] {
        println!("{cmd}");
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        return ExitCode::from(1);
    }
    let cmd = args[0].clone();
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
    let readonly = matches!(cmd.as_str(), "status" | "await" | "idle" | "list" | "hook");
    if !readonly
        && let Err(e) = paths.ensure_dirs()
    {
        err_line(&e.message);
        return ExitCode::from(1);
    }

    let result = match cmd.as_str() {
        "register" => cmd_register(&paths, rest),
        "list" => cmd_list(&paths, rest),
        "send" => cmd_send(&paths, rest),
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
fn cmd_register(paths: &Paths, args: &[String]) -> ab_core::error::Result<()> {
    if args.len() != 2 {
        return Err(Error::new("用法：agent-bridge register <agent> <tmux-target>"));
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
fn cmd_list(paths: &Paths, _args: &[String]) -> ab_core::error::Result<()> {
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
fn cmd_send(paths: &Paths, args: &[String]) -> ab_core::error::Result<()> {
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
                let v = it
                    .next()
                    .ok_or_else(|| Error::new("--message 需要參數"))?;
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

    Err(Error::new(
        "尚未實作：send 成功路徑（M0.5 spike 範圍僅涵蓋分組 2 錯誤路徑，task 建立見 CLI-SEND-1，留待下一階段）",
    ))
}
