//! Claude Code hook 協定的落地端（spec/hooks.md HOOK-ID-*、HOOK-EVT-*、
//! STATE-CHAN-*）。對映 bash `hook_agent_name`:2010、`hook_write_state`:2035、
//! `hook_owner_gate`:2069、`hook_oldest_queued`:2098、`cmd_hook`:2131。
//!
//! **失效方向鐵律**：本模組任何一步出錯（stdin 非 JSON、state 寫入失敗、環境
//! 變數缺失或格式不符）一律以「靜默放行」收場，`run()` 永遠回 `HookOutcome`
//! 而非 `Err`——呼叫端據此無條件 exit 0。exit 2 會被 Claude Code 當成 block
//! 訊號，hook 故障不得干擾 worker 的正常運作。因此本檔幾乎不出現 `?`：每個
//! 失敗路徑都就地收斂成「這一步沒做成，繼續往下」。

use std::path::Path;

use crate::config;
use crate::fsio::atomic_write;
use crate::json::{self, JsonObject};
use crate::paths::Paths;
use crate::task::read_status;
use crate::time::{now_epoch, now_iso, parse_iso_to_epoch};
use crate::validate::{is_valid_name, is_valid_task_id};
use serde_json::Value;

/// `run()` 的結果：hook 只有「印一段 stdout」與「什麼都不印」兩種對外行為。
/// 退出碼恆為 0，故不入型別。
pub enum HookOutcome {
    /// 無 stdout（放行）。
    Silent,
    /// stop 事件要擋下續跑：這段 JSON 印到 stdout。
    Block(String),
}

/// hook_agent_name:2010 — 從 spawn tag 析出「我是誰」。
///
/// tag 格式 `ab-spawn-<name>-<pid>-<12hex>`。name 本身允許連字號與底線
/// （NAME_RE），故不能用第一個 `-` 切。bash 用 `^(.+)-[0-9]+-[0-9a-f]{12}$`
/// 貪婪比對：`.+` 取最長 ⇒ 中間的 `[0-9]+` 取**最短**可行的數字串。這裡逐字
/// 翻譯那個回溯順序（k 由 1 遞增，第一個成立的就是 regex 的答案），而不是
/// 「從尾巴一路吃掉所有數字」——後者對 `my-worker-2-12345-…` 這種名字尾端
/// 帶數字的情形會切錯。
pub fn agent_name_from_tag(tag: &str) -> Option<String> {
    let rest = tag.strip_prefix("ab-spawn-")?;
    let b = rest.as_bytes();
    // 尾端 12 個 hex，其前一個字元是 `-`
    if b.len() < 12 {
        return None;
    }
    let hex_start = b.len() - 12;
    if !b[hex_start..]
        .iter()
        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
    {
        return None;
    }
    if hex_start == 0 || b[hex_start - 1] != b'-' {
        return None;
    }
    let digits_end = hex_start - 1; // exclusive
    // k＝數字串長度，由短而長（對映 `.+` 貪婪的回溯順序）
    for k in 1..digits_end {
        let digits_start = digits_end - k;
        if !b[digits_start..digits_end]
            .iter()
            .all(|c| c.is_ascii_digit())
        {
            break; // 再往左已不是數字，更長的 k 不可能成立
        }
        if b[digits_start - 1] != b'-' {
            continue;
        }
        let name = &rest[..digits_start - 1];
        if name.is_empty() {
            continue;
        }
        // 還原出的名字未過 NAME_RE 一律視為「不是 spawn 出身的呼叫」
        return if is_valid_name(name) {
            Some(name.to_string())
        } else {
            None
        };
    }
    None
}

/// 讀 `AGENT_BRIDGE_SPAWN_TAG` 並析名。無此變數／格式不符一律 `None`
/// （呼叫端靜默 no-op）——主 session／人工註冊 pane 的環境本來就沒有這個
/// 變數，天然不受影響。
pub fn agent_name() -> Option<String> {
    let tag = std::env::var(config::ENV_SPAWN_TAG).ok()?;
    if tag.is_empty() {
        return None;
    }
    agent_name_from_tag(&tag)
}

/// hook_write_state:2035 — state 通道的單一寫者操作（呼叫者是 worker 自己的
/// hook，不取 registry 鎖）。
///
/// `last_delivered = None` 表示保留既有值不動（bash 的 `__KEEP__`）：
/// prompt-submit 與 notification 的 idle 標記都不該動它——它是 Stop hook 用來
/// 辨識「同一個仍待處理的任務是否已被擋過一輪」的依據，被其他事件中途沖掉，
/// 防無限迴圈的判斷就會失準。
///
/// `owner` 逐次明傳、不設保留語意：能走到這裡的寫入者就是正主（或合法接管
/// 者），直接覆寫即可；從舊檔撈反而會在接管當下把舊主的 id 又抄回來。
///
/// 任何一步失敗一律吞掉：失效方向是 state 停在舊值、TTL 到期後退回 legacy
/// 送鍵，不是讓呼叫端跟著非零收場。
pub fn write_state(file: &Path, state: &str, last_delivered: Option<&str>, owner: &str) {
    let ld = match last_delivered {
        Some(v) => v.to_string(),
        None => read_json_field(file, "last_delivered").unwrap_or_default(),
    };
    let doc = JsonObject::new()
        .push_str("state", state)
        .push_str("ts", &now_iso())
        .push_str("last_delivered", &ld)
        .push_str("owner", owner)
        .render();
    let _ = atomic_write(file, format!("{doc}\n").as_bytes());
}

/// 讀某個 JSON 檔的單一欄位，對映 bash `jq -r '.x // empty' … 2>/dev/null || x=""`。
/// 檔案不存在／非 JSON 一律 `None`。state 檔與 registry 都走這支。
///
/// 走 `json::jq_raw_field` 而非 `str_field`：`jq -r` 對非字串值會印出其文字，
/// bash 端拿到的是非空字串。這個差別在 `owner` 上是安全性的——`owner: 1` 在
/// bash 是「有主」（擋異主），用 `str_field` 會變成「無主」（直接放行）。
fn read_json_field(file: &Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(file).ok()?;
    let Ok(Value::Object(fields)) = json::parse(&content) else {
        return None;
    };
    json::jq_raw_field(&fields, key)
}

/// hook_owner_gate:2069 — state 檔所有權閘門，堵「巢狀 runtime 冒用 parent
/// 身分」。回 `true`＝放行（無檔／無主舊格式／本人／可接管）；`false`＝異主
/// 且 state 新鮮，呼叫端須靜默收場且無 stdout（stop 分支不發 block JSON，
/// 巢狀 session 就不會被指使去 receive parent 的任務）。
///
/// 接管條件：state ts 超過 TTL 或落在未來（未來 ts 不可信，比照
/// `notify_or_defer` 的下界論證）。ts 過期時通知端本來就把這份 state 當
/// 「未知」走 legacy——通道此刻已降級，交出所有權零損失。
///
/// **M5（HOOK-OWNER-5）**：行程身分只做一件事——確認得了本尊就即刻放行，
/// `/clear` 的自癒因此變成即時（行程沒變＝身分沒變），不再等 TTL。確認不了
/// 就整條交還給下面的 session_id＋TTL，也就是 M4 行為；這條路上沒有「確認為
/// 冒名」這種結論。
pub fn owner_gate(file: &Path, sid: &str, reg_file: &Path) -> bool {
    // 確認是 worker 本尊→即刻放行，不看 sid/ts（`/clear` 換 sid 後的自癒）。
    // 確認不了→什麼都不主張，照 M4 的 session_id＋TTL 走
    if proc_identity_confirms_self(reg_file) {
        return true;
    }
    let Some(owner) = read_json_field(file, "owner") else {
        // 檔案不存在、非 JSON、或無 owner 欄（舊格式）一律放行
        return true;
    };
    if owner == sid {
        return true;
    }
    let ttl = config::state_ttl_lenient();
    // ts 壞掉／缺欄位：這份 state 已不可信，視同過期、允許接管（與
    // notify_or_defer 把解析失敗當「未知」同向）。
    let Some(ts) = read_json_field(file, "ts") else {
        return true;
    };
    let Some(epoch) = parse_iso_to_epoch(&ts) else {
        return true;
    };
    let age = now_epoch() - epoch;
    !(age >= 0 && age <= ttl)
}

/// HOOK-OWNER-5 的行程身分裁決：`true`＝已確認本 hook 是 registry 記下的那個
/// worker runtime 自己（含其自有啟動鏈）fork 的；`false`＝**確認不了**（呼叫
/// 端落回 session_id＋TTL），不是「確認為冒名」。
///
/// 確認形狀**逐 runtime 白名單**，不是泛用祖先鏈：直接形（PPID ==
/// `worker_pid`，claude 實測）對所有 runtime 都試；launcher 形（PPID 的父行程
/// == `worker_pid` 且 PPID 命中 codex argv 形，恰好一層）只在 registry
/// `runtime=codex` 時試。MUST NOT 放寬成「祖先鏈走得到 worker_pid」或「鏈上
/// 沒夾別的 runtime 形狀」：實測巢狀鏈（m5-proposal §1）是 hook → 巢狀
/// claude → bash → 本尊——巢狀正是 hook 的直接 PPID、其餘中介只有 bash，兩種
/// 放寬都會把它確認成本尊，而誤認在本函式的代價是**立即奪權**（不經 TTL），
/// 是唯一比 M4 更糟的失效方向（architecture §11.7）。
///
/// **只有 exact match 是安全的 positive**。PPID 與記錄的 pid 不符不能當成冒名
/// ——合法的中介一樣會不符：runtime fork 出新的主行程而舊主還活著、或使用者經
/// `AGENT_BRIDGE_CLAUDE_HOOKS`（公開覆蓋面）指定了 hook wrapper。把不符當成
/// 確定的冒名，會讓本尊被**永久**誤擋，連 TTL 自癒都到不了，正是架構 §11.2
/// 自己定的「比未實作更糟」（codex 複核 2026-07-31 §1）。
///
/// 代價是放棄「冒名者過了 TTL 仍永遠擋」這個收緊：M5 收斂成純粹的本尊即時
/// 自癒，其餘一律等於 M4 行為，失效方向這才真正對稱。
///
/// 取樣順序是 **PPID 先、中介次之、`worker_pid` 的 starttime 最後**：反過來
/// 的話父行程若在兩次讀取之間退出，hook 會被 reparent，於是前一步證實了
/// recorded process、後一步卻拿到新的 PPID（codex 複核 §2 blocker 2）。先讀
/// PPID 則走訪途中任何行程退出只會讓後續讀取失敗或 starttime 對不上而落回
/// ——TOCTOU 的失效方向安全。
fn proc_identity_confirms_self(reg_file: &Path) -> bool {
    if !crate::proc::available() {
        return false;
    }
    let (Some(pid), Some(recorded_start)) = (
        read_json_field(reg_file, "worker_pid"),
        read_json_field(reg_file, "worker_starttime"),
    ) else {
        return false;
    };
    if pid.is_empty() || recorded_start.is_empty() {
        return false;
    }
    // 誰 fork 了這個 hook。讀不到就什麼都不主張
    let Some(p1) = crate::proc::self_ppid() else {
        return false;
    };
    if p1 != pid && !launcher_hop_reaches(&p1, &pid, reg_file) {
        return false;
    }
    // pid 會被回收重用：worker 得是記錄當下的那一次行程生命，才算本尊
    crate::proc::starttime(&pid).as_deref() == Some(recorded_start.as_str())
}

/// launcher 形（HOOK-OWNER-5）：`p1`（hook 的直接 PPID）是不是 `worker_pid`
/// fork 出的同產品原生執行檔——恰好一層，只在 registry `runtime=codex` 時嘗試
/// （實測形狀：`docs/codex-hooks-probe.md` 補測節）。
///
/// 對 p1 的三個讀取（cmdline、父 pid、starttime）必須釘在同一次行程生命：
/// attest 以 starttime 夾住 cmdline，父 pid 與 starttime 出自同一次 stat 讀取，
/// 再要求兩邊 starttime 相等。缺了這一環，p1 在兩讀之間退出且 pid 被重用時，
/// argv 驗的是舊行程、父 pid 記的是新行程。
fn launcher_hop_reaches(p1: &str, worker_pid: &str, reg_file: &Path) -> bool {
    if read_json_field(reg_file, "runtime").as_deref() != Some("codex") {
        return false;
    }
    let Some(attested_start) = crate::proc::attest_runtime(p1, "codex") else {
        return false;
    };
    let Some((gp, start_now)) = crate::proc::ppid_and_starttime(p1) else {
        return false;
    };
    start_now == attested_start && gp == worker_pid
}

/// hook_oldest_queued:2098 — 最舊一筆 `to == agent` 且狀態為 queued 的 task-id。
///
/// task-id 以 UTC 時間戳起首，字典序即時間序，不必轉 epoch 排序。狀態一律讀
/// 裸 status 檔而非 metadata 的 status 欄——status 檔是狀態機的權威，兩者短暫
/// 不一致時這裡不能算錯。
///
/// `tasks/` 是同互信域內每個 worker 都可寫的目錄，目錄名未經 TASK_ID_RE 就
/// 直接餵給 block reason 裡的 `agent-bridge receive <id>`，會進到另一個 worker
/// 模型看到的指令語意空間。壞掉或惡意的目錄名在此就地跳過、不得進入候選——
/// 與 check_task_id 同級把關，只是不 die：hook 鐵律是異常不得中止，跳過這筆
/// 繼續掃下一筆。
pub fn oldest_queued(paths: &Paths, agent: &str) -> Option<String> {
    let entries = std::fs::read_dir(&paths.tasks_dir).ok()?;
    let mut oldest: Option<String> = None;
    for entry in entries.flatten() {
        let dir = entry.path();
        let meta = dir.join("metadata.json");
        if !meta.is_file() {
            continue;
        }
        let Some(id) = dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_valid_task_id(id) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&meta) else {
            continue;
        };
        let Ok(Value::Object(fields)) = json::parse(&content) else {
            continue;
        };
        if json::jq_raw_field(&fields, "to").as_deref() != Some(agent) {
            continue;
        }
        match read_status(&dir) {
            Ok(st) if st == "queued" => {}
            _ => continue,
        }
        if oldest.as_deref().is_none_or(|o| id < o) {
            oldest = Some(id.to_string());
        }
    }
    oldest
}

/// stdin 讀取上限（秒）。bash 用 `timeout 2 cat`；理由同：裸讀沒有上限，
/// fd 0 已關閉或管線開著卻不送資料時程序會卡住不返回，Claude Code 的 Stop
/// hook 呼叫端因此被無限期擋住，worker 的 turn 永遠結束不了。這是「hook 故障
/// 不得干擾 worker」這條鐵律唯一沒有 TTL 能救的一段，只能靠讀取本身設上限。
const STDIN_TIMEOUT_SECS: u64 = 2;

/// 讀 stdin，逾時或讀不到一律當空字串。逾時後讀取執行緒仍掛在 `read_to_end`
/// 上，但本行程隨即 exit 0，不需要（也沒有辦法在 std 內）中斷它。
fn read_stdin_bounded() -> String {
    use std::io::Read;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let ok = std::io::stdin().read_to_end(&mut buf).is_ok();
        let _ = tx.send(if ok { buf } else { Vec::new() });
    });
    let strip_nul = |buf: Vec<u8>| -> Vec<u8> {
        // bash 是 `stdin_json="$(timeout 2 cat)"`——command substitution 會把
        // NUL 位元組整個丟掉，jq 於是收到一份剝過 NUL 的（仍合法的）JSON。
        // 不剝的話 serde 會拒絕整份文件，同一個 payload 在兩邊走向不同分支
        // （codex 複核 2026-07-31）。
        if buf.contains(&0) {
            buf.into_iter().filter(|b| *b != 0).collect()
        } else {
            buf
        }
    };
    match rx
        .recv_timeout(std::time::Duration::from_secs(STDIN_TIMEOUT_SECS))
        .map(strip_nul)
    {
        // 非 UTF-8 payload 走 lossy 而非中止：後續每個欄位查詢都有保底分支，
        // 亂碼只會讓查詢落空，維持靜默放行
        Ok(buf) => String::from_utf8_lossy(&buf).into_owned(),
        Err(_) => String::new(),
    }
}

/// cmd_hook:2131 — `hook <stop|prompt-submit|notification>` 的本體。
/// 永遠回 `HookOutcome`（不回 `Err`）：呼叫端無條件 exit 0。
pub fn run(paths: &Paths, args: &[String]) -> HookOutcome {
    let event = args.first().map(String::as_str).unwrap_or("");
    // 未知事件：靜默放行，避免 Claude Code 日後新增事件把 worker 卡住
    if !matches!(event, "stop" | "prompt-submit" | "notification") {
        return HookOutcome::Silent;
    }

    let Some(name) = agent_name() else {
        return HookOutcome::Silent;
    };

    // 這一步在析名之後：無 tag 的呼叫不得留下任何痕跡（含 state/ 目錄本身）
    if std::fs::create_dir_all(&paths.state_dir).is_err() {
        return HookOutcome::Silent;
    }
    let state_file = paths.state_dir.join(format!("{name}.json"));
    // HOOK-OWNER-5 的行程身分來源。**只讀不寫**：hook 從來不碰 registry，
    // 這裡也不建立它——讀不到就是落回時間窗
    let reg_file = paths.agents_dir.join(format!("{name}.json"));

    let stdin_json = read_stdin_bounded();
    let fields = match json::parse(&stdin_json) {
        Ok(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };

    // 所有權閘門（三事件一體適用，必須在 stop 分支查 mailbox／發 block 之前）：
    // 缺 session_id 的 payload 不參與 state 通道——「無身分者不寫、不 block」，
    // 與無 SPAWN_TAG 的 no-op 同構；失效方向就是既有降級鏈（state 停更 →
    // TTL → 退回 legacy 送鍵）。若改成「不驗照舊寫」，任何缺欄位的巢狀呼叫
    // 會把冒名攻擊面原樣打開。
    let Some(sid) = json::jq_raw_field(&fields, "session_id") else {
        return HookOutcome::Silent;
    };
    if !owner_gate(&state_file, &sid, &reg_file) {
        return HookOutcome::Silent;
    }

    match event {
        "prompt-submit" => {
            write_state(&state_file, "busy", None, &sid);
            HookOutcome::Silent
        }
        "notification" => {
            // 判別欄位為 `notification_type`（值如 idle_prompt／
            // permission_prompt）。已知缺口：實際 payload 有時完全缺這個欄位
            // （claude-code issue #12048）。對此 fail-safe：缺欄位落入「其他
            // 型別不動 state」分支，不是誤判成 idle——代價是 state 停舊值，
            // TTL 到期後仍會退回 legacy 送鍵，不會永久卡住。
            if json::jq_raw_field(&fields, "notification_type").as_deref() == Some("idle_prompt") {
                write_state(&state_file, "idle", None, &sid);
            }
            HookOutcome::Silent
        }
        _ => run_stop(paths, &state_file, &name, &fields, &sid),
    }
}

/// bash `[[ "$(jq -r '.stop_hook_active // false')" == "true" ]]` 的逐字語意：
/// `jq -r` 把布林 `true` 與字串 `"true"` 都印成 `true`，兩者在 bash 端無從
/// 分辨、都算真；數字 `1`、`null`、缺欄位一律不算。
///
/// 不借用 `json::bool_field_is_true`（那是 `== true` 的嚴格比較語意，字串
/// `"true"` 會被判否）——同類的寬鬆／嚴格混用正是 parity 陷阱。
fn jq_r_is_true(fields: &serde_json::Map<String, Value>, key: &str) -> bool {
    match fields.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

fn run_stop(
    paths: &Paths,
    state_file: &Path,
    name: &str,
    fields: &serde_json::Map<String, Value>,
    sid: &str,
) -> HookOutcome {
    let stop_active = jq_r_is_true(fields, "stop_hook_active");
    let Some(pending) = oldest_queued(paths, name) else {
        write_state(state_file, "idle", None, sid);
        return HookOutcome::Silent;
    };

    let last_delivered = read_json_field(state_file, "last_delivered").unwrap_or_default();
    if stop_active && pending == last_delivered {
        // 同一個仍待處理的 task 已經被擋過一輪、模型還是選擇停下：這才是真
        // 迴圈的訊號（模型不理會 reason），一輪就放行，不再無限擋。多任務
        // 合法連鎖時 pending 會是「下一個」不同 id，走下面的 block 分支，
        // worker 可以一路把 mailbox 清空。
        write_state(state_file, "idle", None, sid);
        return HookOutcome::Silent;
    }

    write_state(state_file, "busy", Some(&pending), sid);
    let reason = format!(
        "New task {pending} is in your agent-bridge mailbox. Run: agent-bridge receive {pending} and handle it per your worker brief."
    );
    let doc = JsonObject::new()
        .push_str("decision", "block")
        .push_str("reason", &reason)
        .render();
    HookOutcome::Block(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_parsing_handles_hyphenated_names() {
        assert_eq!(
            agent_name_from_tag("ab-spawn-zoe-12345-0123456789ab").as_deref(),
            Some("zoe")
        );
        // 名字含連字號且尾端帶數字：貪婪切點必須落在最後一段數字之前
        assert_eq!(
            agent_name_from_tag("ab-spawn-my-worker-2-12345-0123456789ab").as_deref(),
            Some("my-worker-2")
        );
        assert_eq!(
            agent_name_from_tag("ab-spawn-a_b-7-aaaaaaaaaaaa").as_deref(),
            Some("a_b")
        );
    }

    #[test]
    fn tag_parsing_rejects_malformed() {
        // 缺前綴
        assert_eq!(agent_name_from_tag("spawn-zoe-1-0123456789ab"), None);
        // hex 段長度不對
        assert_eq!(agent_name_from_tag("ab-spawn-zoe-1-0123456789a"), None);
        // hex 段含非 hex 字元
        assert_eq!(agent_name_from_tag("ab-spawn-zoe-1-0123456789aZ"), None);
        // 缺 pid 段
        assert_eq!(agent_name_from_tag("ab-spawn-zoe-0123456789ab"), None);
        // 名字為空
        assert_eq!(agent_name_from_tag("ab-spawn--1-0123456789ab"), None);
        // 名字未過 NAME_RE
        assert_eq!(
            agent_name_from_tag("ab-spawn-bad name-1-0123456789ab"),
            None
        );
        assert_eq!(agent_name_from_tag(""), None);
    }

    /// owner 欄缺失（舊格式 state 檔）必須放行，否則升級當下所有既有 worker
    /// 的 hook 會一起啞掉。
    #[test]
    fn owner_gate_allows_ownerless_and_self() {
        let dir = std::env::temp_dir().join(format!("ab-hook-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s.json");
        // 這一組測的是**落回路徑**（HOOK-OWNER-2 的 session_id＋TTL）：registry
        // 不存在 ⇒ 沒有行程身分可用。M5 的身分分支由 proc_identity_* 各測例覆蓋
        let no_reg = dir.join("no-such-registry.json");

        // 檔案不存在
        assert!(owner_gate(&f, "s1", &no_reg));

        // 無 owner 欄
        std::fs::write(&f, br#"{"state":"idle","ts":"2020-01-01T00:00:00Z"}"#).unwrap();
        assert!(owner_gate(&f, "s1", &no_reg));

        // 本人
        write_state(&f, "busy", Some("t1"), "s1");
        assert!(owner_gate(&f, "s1", &no_reg));

        // 異主且新鮮 → 擋
        assert!(!owner_gate(&f, "other", &no_reg));

        // owner 是非字串但 `jq -r` 印得出東西（number／true／物件）＝有主，
        // 異主且新鮮就得擋。用 str_field 會把它讀成「無主」而放行，那正是
        // 巢狀 runtime 冒用 parent 身分的入口（codex 複核 2026-07-31）
        for owner_json in [r#"1"#, r#"true"#, r#"["a"]"#] {
            let doc = format!(
                r#"{{"state":"busy","ts":"{}","owner":{owner_json}}}"#,
                crate::time::now_iso()
            );
            std::fs::write(&f, doc.as_bytes()).unwrap();
            assert!(
                !owner_gate(&f, "nested", &no_reg),
                "owner={owner_json} 應視為有主"
            );
        }
        // `false` 與 `null` 在 jq 的 `//` 之下都落到 empty＝無主
        for owner_json in [r#"false"#, r#"null"#] {
            let doc = format!(
                r#"{{"state":"busy","ts":"{}","owner":{owner_json}}}"#,
                crate::time::now_iso()
            );
            std::fs::write(&f, doc.as_bytes()).unwrap();
            assert!(
                owner_gate(&f, "nested", &no_reg),
                "owner={owner_json} 應視為無主"
            );
        }

        // 異主但 ts 過期 → 可接管
        std::fs::write(
            &f,
            br#"{"state":"busy","ts":"2020-01-01T00:00:00Z","owner":"s1"}"#,
        )
        .unwrap();
        assert!(owner_gate(&f, "other", &no_reg));

        // 異主但 ts 落在未來 → 不可信，可接管
        std::fs::write(
            &f,
            br#"{"state":"busy","ts":"2999-01-01T00:00:00Z","owner":"s1"}"#,
        )
        .unwrap();
        assert!(owner_gate(&f, "other", &no_reg));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// HOOK-OWNER-5：行程身分在場時凌駕時間窗，且「不確定」一律落回。
    #[test]
    fn proc_identity_overrides_the_time_window_and_falls_back_when_unsure() {
        let dir = std::env::temp_dir().join(format!("ab-hook-proc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("s.json");
        let reg = dir.join("r.json");
        // 異主且新鮮：沒有行程身分的話這是「擋」
        write_state(&state, "busy", Some("t1"), "s1");

        let ppid = crate::proc::self_ppid().expect("/proc 應可讀");
        let ppid_start = crate::proc::starttime(&ppid).expect("父行程 starttime 應可讀");
        let write_reg = |pid: &str, start: &str| {
            std::fs::write(
                &reg,
                format!(r#"{{"worker_pid":"{pid}","worker_starttime":"{start}"}}"#),
            )
            .unwrap();
        };

        // 本尊：直接 PPID 與 starttime 都對上 → 放行，且不看 session_id
        write_reg(&ppid, &ppid_start);
        assert!(
            owner_gate(&state, "brand-new-session", &reg),
            "PPID 相符時應無視異主的 session_id"
        );

        // PPID 不符（巢狀 runtime、或合法的中介／新主行程——這裡分不出來）：
        // 不主張任何事，落回 session_id＋TTL。state 仍新鮮且異主，故照舊擋
        let me = std::process::id().to_string();
        let me_start = crate::proc::starttime(&me).unwrap();
        write_reg(&me, &me_start);
        assert!(!owner_gate(&state, "nested", &reg), "異主且 state 新鮮應擋");

        // 但擋是「時間窗擋的」，不是身分擋的：ts 一過期就該能接管。**不得**
        // 把 PPID 不符當成確定的冒名——合法中介一樣不符，永久擋死本尊比不做
        // 還糟（codex 複核 §1）
        std::fs::write(
            &state,
            br#"{"state":"busy","ts":"2020-01-01T00:00:00Z","owner":"s1"}"#,
        )
        .unwrap();
        assert!(
            owner_gate(&state, "nested", &reg),
            "PPID 不符只是確認不了，過期後仍須可接管"
        );

        // PPID 相符但 starttime 對不上（pid 被回收重用）：記錄過期，同樣落回
        // 時間窗——此刻 ts 已過期故可接管
        write_reg(&ppid, "999999999999");
        assert!(
            owner_gate(&state, "other", &reg),
            "記錄過期應落回時間窗而非擋死"
        );

        // 欄位缺其一、空值、registry 不存在 → 一律落回
        for doc in [
            r#"{"worker_pid":"1"}"#,
            r#"{"worker_starttime":"1"}"#,
            r#"{"worker_pid":"","worker_starttime":""}"#,
            r#"{}"#,
        ] {
            std::fs::write(&reg, doc.as_bytes()).unwrap();
            assert!(owner_gate(&state, "other", &reg), "doc={doc} 應落回");
        }
        assert!(owner_gate(&state, "other", &dir.join("nope.json")));

        // launcher 形的落回面：worker_pid 指向**祖父**行程（差一層，形狀上
        // 等同 launcher 情境），但——
        // ① registry 無 runtime 欄／runtime 不是 codex（含 claude）：一層之
        //    隔不得確認。這是防「巢狀 claude 直接掛在本尊下」誤放行的錨：
        //    launcher 形只准 codex 白名單啟用（architecture §11.7）
        // ② runtime=codex 但中介（本測試的真實父行程）argv 不命中 codex 形：
        //    同樣不得確認
        // state 回到新鮮異主，落回時間窗＝擋
        write_state(&state, "busy", Some("t1"), "s1");
        let (gp, gp_start) = crate::proc::ppid_and_starttime(&ppid).expect("祖父行程應可讀");
        for runtime_json in ["", r#","runtime":"claude""#, r#","runtime":"codex""#] {
            std::fs::write(
                &reg,
                format!(r#"{{"worker_pid":"{gp}","worker_starttime":"{gp_start}"{runtime_json}}}"#),
            )
            .unwrap();
            assert!(
                !owner_gate(&state, "nested", &reg),
                "runtime_json={runtime_json:?} 隔一層不得確認，應落回時間窗而擋"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `__KEEP__` 語意：不給 last_delivered 時保留舊值，owner 照樣覆寫。
    #[test]
    fn write_state_keeps_last_delivered() {
        let dir = std::env::temp_dir().join(format!("ab-hook-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s.json");

        write_state(&f, "busy", Some("task-1"), "s1");
        assert_eq!(
            read_json_field(&f, "last_delivered").as_deref(),
            Some("task-1")
        );

        write_state(&f, "idle", None, "s2");
        assert_eq!(read_json_field(&f, "state").as_deref(), Some("idle"));
        assert_eq!(
            read_json_field(&f, "last_delivered").as_deref(),
            Some("task-1")
        );
        assert_eq!(read_json_field(&f, "owner").as_deref(), Some("s2"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
