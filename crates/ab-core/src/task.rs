//! task 目錄結構、id 生成／驗證、狀態機轉換與 events.log（state.md STATE-TASK-*、
//! cli.md 轉換條款）。對映 bash `write_message`:276、`log_event`:251、
//! `update_meta_status`:261、`check_task_id`:420、`send_rollback`:202、
//! `cmd_send`:536 的 task 建立段。

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fsio::atomic_write;
use crate::json;
use crate::paths::Paths;
use crate::time::{now_compact, now_iso};
use crate::validate::is_valid_task_id;
use serde_json::{Map, Value};

/// 磁碟表現＝小寫字串（`status` 檔與 metadata.json 逐字同 bash）。狀態機的
/// 合法轉換不寫成集中式轉換表：bash 的每個指令各有自己的可接受集合**與各自
/// 逐字不同的拒絕訊息**（receive／start／reply／cancel 四種措辭），集中表反而
/// 要再長一張訊息對映表，故轉換條件留在各 `cmd_*` 對應函式內就近判斷。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Delivered,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Delivered => "delivered",
            TaskState::Running => "running",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        )
    }

    /// 權威狀態字 → `TaskState`；不在這六個字之內一律 `None`。
    ///
    /// `tasks/` 裡任何人都能寫 status 檔，而 dashboard 的 status 軸 MUST 只顯示
    /// 權威字（tui-design.md §2：不存在 `blocked` 這個 task 狀態）。沒有這道
    /// 驗證，一個手寫的 `blocked` 就會原字上畫面，並被當成非終態而開得了
    /// cancel 確認框。
    pub fn parse(s: &str) -> Option<TaskState> {
        match s {
            "queued" => Some(TaskState::Queued),
            "delivered" => Some(TaskState::Delivered),
            "running" => Some(TaskState::Running),
            "completed" => Some(TaskState::Completed),
            "failed" => Some(TaskState::Failed),
            "cancelled" => Some(TaskState::Cancelled),
            _ => None,
        }
    }
}

/// 訊息來源：`--message <text>`／`--message-file <path>`／`--message-file -`（stdin）。
/// 對映 bash `write_message` 的 `mode`/`val` 二元組。
///
/// `Text` 帶的是 **argv 的原始位元組**而非 `String`（M3 修）：`--message` 的
/// 值是 payload 的一部分，分組 6 驗 byte-for-byte 保真，而 bash 的
/// `printf '%s\n' "$val"` 原樣吐出呼叫端給的位元組。經 `String` 中轉會把非
/// UTF-8 位元組換成 U+FFFD——payload 邊界走 bytes 是架構 §3 的紅線。
#[derive(Debug, Clone)]
pub enum MessageSource {
    Text(Vec<u8>),
    File(PathBuf),
    Stdin,
}

/// check_task_id:420 — 文法不合即 die，訊息逐字對齊。
pub fn check_task_id(id: &str) -> Result<()> {
    if is_valid_task_id(id) {
        Ok(())
    } else {
        Err(Error::new(format!("task-id 不合法：{id}")))
    }
}

pub fn task_dir(paths: &Paths, id: &str) -> PathBuf {
    paths.tasks_dir.join(id)
}

/// 目錄存在性檢查，訊息逐字對齊 bash `[[ -d "$dir" ]] || die "找不到 task：$id"`。
pub fn require_task_dir(paths: &Paths, id: &str) -> Result<PathBuf> {
    let dir = task_dir(paths, id);
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(Error::new(format!("找不到 task：{id}")))
    }
}

/// 讀裸 status 檔。**裸 status 是操作上的權威**（update_meta_status:265-270 的
/// 寫入順序註記），狀態機與 await 全數讀它，不讀 metadata.status。
pub fn read_status(dir: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(dir.join("status"))
        .map_err(|e| Error::new(format!("無法讀取 status 檔：{e}")))?;
    Ok(raw.trim_end_matches('\n').to_string())
}

/// log_event:251 — append-only events.log，行格式 `<iso> <event>[ <detail>]`。
pub fn log_event(paths: &Paths, id: &str, event: &str, detail: &str) -> Result<()> {
    use std::io::Write;
    let mut line = format!("{} {event}", now_iso());
    if !detail.is_empty() {
        line.push(' ');
        line.push_str(detail);
    }
    line.push('\n');
    let path = task_dir(paths, id).join("events.log");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| Error::new(format!("無法開啟 events.log {}：{e}", path.display())))?;
    f.write_all(line.as_bytes())
        .map_err(|e| Error::new(format!("無法寫入 events.log {}：{e}", path.display())))
}

/// update_meta_status:261 — 同步更新 metadata.json 與裸 status 檔。
/// **寫入順序不可對調**：先裸 status、後 metadata（bash:265-273 的論證——中途
/// 被 SIGKILL 時，殘留分歧只會是 metadata.status 落後一步的展示性資訊；反過來
/// 則是終態轉換可被重放）。
pub fn update_meta_status(dir: &Path, new: TaskState) -> Result<()> {
    let ts = now_iso();
    let meta_path = dir.join("metadata.json");
    let content = std::fs::read_to_string(&meta_path).map_err(|e| {
        Error::new(format!(
            "無法讀取 metadata.json {}：{e}",
            meta_path.display()
        ))
    })?;
    let mut fields = match json::parse(&content) {
        Ok(Value::Object(fields)) => fields,
        _ => {
            return Err(Error::new(format!(
                "metadata.json 不是合法的 JSON 物件：{}",
                meta_path.display()
            )));
        }
    };
    // jq `.status = $s | .updated_at = $t`：既有鍵原位改值，欄位序不動。
    json::set_str_field(&mut fields, "status", new.as_str());
    json::set_str_field(&mut fields, "updated_at", &ts);
    let doc = format!("{}\n", json::render_pretty(&Value::Object(fields)));

    atomic_write(
        &dir.join("status"),
        format!("{}\n", new.as_str()).as_bytes(),
    )?;
    atomic_write(&meta_path, doc.as_bytes())
}

/// write_message:276 — payload 走 byte 流原樣搬運（分組 6 驗 byte-for-byte
/// 保真，架構 §3）。`Text` 模式比照 `printf '%s\n'` 補一個換行；檔案／stdin
/// 模式原樣搬，不補不刪。
pub fn write_message(dest: &Path, src: &MessageSource) -> Result<()> {
    let bytes: Vec<u8> = match src {
        MessageSource::Text(t) => {
            let mut b = t.clone();
            b.push(b'\n');
            b
        }
        MessageSource::Stdin => {
            let mut b = Vec::new();
            std::io::stdin()
                .read_to_end(&mut b)
                .map_err(|e| Error::new(format!("無法讀取 stdin：{e}")))?;
            b
        }
        MessageSource::File(p) => {
            // 第二道存在性檢查（cmd_send:559-562 的註記：預檢與此處之間的
            // TOCTOU 空隙內檔案可能被刪），訊息逐字對齊。
            if !p.is_file() {
                return Err(Error::new(format!("找不到訊息檔：{}", p.display())));
            }
            std::fs::read(p)
                .map_err(|e| Error::new(format!("找不到訊息檔：{}（{e}）", p.display())))?
        }
    };
    atomic_write(dest, &bytes)
}

/// 讀 metadata.json 的字串欄位。
///
/// **缺欄位／JSON null 回字面 `"null"`**，對映 bash `jq -r '.from'` 的輸出：
/// receive／read 的 stderr 標頭會逐字印出這個值，而 `respond_task` 拿它去找
/// `agents/<from>.json`——兩邊都要與 bash 同一個字面值，否則損壞 metadata 下
/// 的行為會分岔（codex M1 審查提出、本輪裁決採納對齊 bash）。
///
/// 已知殘留差異：欄位存在但型別非字串（例如數字）時 jq 會印該值的 JSON
/// 表示，此處回空字串。metadata 全由本工具寫出、這些欄位恆為字串，M1 不為
/// 這個路徑加碼；真要收斂應與 `--contract-manifest` 的形狀討論一起做。
///
/// metadata 本身讀不到或不是 object 則回 `Err`（bash 在 `set -e` 下由 jq
/// 的非零退出達到同樣效果）。
pub fn meta_str(dir: &Path, key: &str) -> Result<String> {
    let meta_path = dir.join("metadata.json");
    let content = std::fs::read_to_string(&meta_path).map_err(|e| {
        Error::new(format!(
            "無法讀取 metadata.json {}：{e}",
            meta_path.display()
        ))
    })?;
    match json::parse(&content) {
        Ok(Value::Object(fields)) => Ok(match fields.get(key) {
            Some(Value::String(s)) => s.clone(),
            None | Some(Value::Null) => "null".to_string(),
            Some(_) => String::new(),
        }),
        _ => Err(Error::new(format!(
            "metadata.json 不是合法的 JSON 物件：{}",
            meta_path.display()
        ))),
    }
}

/// `last_task_at`:457 — 最後一個派給該 agent 的任務建立時間（ISO），從未被
/// 派工則回空字串。掃 `tasks/` 而不是在 send 時記進 registry：send 是核心
/// 路徑，加一次「取鎖＋寫入」會擴大它的失敗面，而這資訊只有回收決策要用。
///
/// ISO 8601 UTC 定長字串可直接字典序比大小，不必轉 epoch。單一損壞的
/// metadata 不該讓整份報表消失——跳過它，繼續看其他任務（bash 的
/// `|| continue`）。欄位取值走 `jq -r` 語意（`jq_raw_field`）。
pub fn last_task_at(paths: &Paths, who: &str) -> String {
    let Ok(rd) = std::fs::read_dir(&paths.tasks_dir) else {
        return String::new();
    };
    let mut latest = String::new();
    for entry in rd.filter_map(|e| e.ok()) {
        let meta_path = entry.path().join("metadata.json");
        if !meta_path.is_file() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(Value::Object(fields)) = json::parse(&content) else {
            continue;
        };
        if json::jq_raw_field(&fields, "to").as_deref() != Some(who) {
            continue;
        }
        let Some(ts) = json::jq_raw_field(&fields, "created_at") else {
            continue;
        };
        if latest.is_empty() || ts > latest {
            latest = ts;
        }
    }
    latest
}

/// rand_suffix:146 — `head -c 2 /dev/urandom | od -An -tx1`：2 bytes 轉 4 位
/// 小寫 hex。std-only（架構 §1），直接讀 `/dev/urandom`；讀不到時退回
/// pid＋奈秒混合值——task-id 的隨機尾綴只為避免同秒碰撞，不是安全用途，
/// 且 `cmd_send` 本來就有碰撞重試迴圈兜底。
pub(crate) fn rand_suffix() -> String {
    let mut buf = [0u8; 2];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom")
        && f.read_exact(&mut buf).is_ok()
    {
        return format!("{:02x}{:02x}", buf[0], buf[1]);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{:04x}", (nanos ^ std::process::id()) & 0xffff)
}

/// task 目錄建立到「完整形狀」（metadata＋status 都寫齊）之間的守衛。
/// 對映 bash 的 `SEND_RB_DIR`＋EXIT trap（send_rollback:202）：中途任何失敗都
/// 要把殘缺目錄收走——沒有 metadata/status 的目錄不屬於任何狀態、gc 又刻意只
/// 碰完整形狀，留下就是永久孤兒。`commit()` 之後才解除。
struct SendRollback {
    dir: Option<PathBuf>,
}

impl SendRollback {
    fn commit(mut self) {
        self.dir = None;
    }
}

impl Drop for SendRollback {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take()
            && std::fs::remove_dir_all(&dir).is_err()
        {
            eprintln!(
                "agent-bridge: 警告：殘缺 task 目錄清除失敗，請手動移除：{}",
                dir.display()
            );
        }
    }
}

/// cmd_send:576-611 的 task 建立段：碰撞重試建目錄 → 寫 request → 寫
/// metadata → 寫 status → 記 created 事件。回傳 task-id。
///
/// 這裡的回滾用 `Drop`（與架構 §6 對鎖的紅線不同）是有意的：鎖的紅線是
/// 「殘鎖行為必須與 bash 等價」，而 bash 的回滾本身就是 EXIT trap——同樣不
/// 在 SIGKILL 下執行。兩者的失效邊界一致，故此處 Drop 是等價實作而非弱化。
pub fn create_task(
    paths: &Paths,
    from: &str,
    to: &str,
    src: &MessageSource,
    pinned: bool,
) -> Result<String> {
    let mut tries = 0;
    let (task_id, dir) = loop {
        let id = format!("{}-{}", now_compact(), rand_suffix());
        let d = paths.tasks_dir.join(&id);
        if std::fs::create_dir(&d).is_ok() {
            break (id, d);
        }
        tries += 1;
        if tries >= 5 {
            return Err(Error::new("無法建立 task 目錄（task-id 連續碰撞）"));
        }
    };
    let rb = SendRollback {
        dir: Some(dir.clone()),
    };

    // die 文字**逐字**對齊 bash:593-594／606-609，不得附加底層錯誤原因：
    // 這三句是 CLI 契約面，尾巴多一個「：{io error}」就不是同一句話了。
    write_message(&dir.join("request.md"), src)
        .map_err(|_| Error::new("send 無法寫入 request（殘缺目錄已回滾）"))?;

    let ts = now_iso();
    let wd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // 欄位序逐字對齊 bash `jq -n '{version, task_id, from, to, created_at,
    // updated_at, working_directory, status} + (pinned)'`（cmd_send:599-604）；
    // `preserve_order` 下 Map 依插入序輸出。
    let mut fields = Map::new();
    fields.insert("version".into(), Value::from(1));
    fields.insert("task_id".into(), Value::from(task_id.clone()));
    fields.insert("from".into(), Value::from(from));
    fields.insert("to".into(), Value::from(to));
    fields.insert("created_at".into(), Value::from(ts.clone()));
    fields.insert("updated_at".into(), Value::from(ts));
    fields.insert("working_directory".into(), Value::from(wd));
    fields.insert("status".into(), Value::from("queued"));
    if pinned {
        fields.insert("pinned".into(), Value::Bool(true));
    }
    let doc = format!("{}\n", json::render_pretty(&Value::Object(fields)));
    atomic_write(&dir.join("metadata.json"), doc.as_bytes())
        .map_err(|_| Error::new("send 無法寫入 metadata（殘缺目錄已回滾）"))?;
    atomic_write(&dir.join("status"), b"queued\n")
        .map_err(|_| Error::new("send 無法寫入 status（殘缺目錄已回滾）"))?;
    rb.commit();

    log_event(paths, &task_id, "created", &format!("from={from} to={to}"))?;
    Ok(task_id)
}

/// gc 一輪的統計。欄位對映 bash `cmd_gc` 的計數器（kept_live／kept_pin／
/// kept_proof／kept_young／removed／failed）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcStats {
    pub kept_live: usize,
    pub kept_pin: usize,
    pub kept_proof: usize,
    pub kept_young: usize,
    pub removed: usize,
    pub failed: usize,
    /// 試算模式的待刪清單 `(id, status, created_at)`；`apply` 時為空。
    pub candidates: Vec<(String, String, String)>,
}

/// send 生成的 task 目錄名形狀：`^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{4}$`。
///
/// **這裡刻意不用寬鬆的 `TASK_ID_RE`**（cmd_gc:1722-1727）：公開驗證用的
/// TASK_ID_RE 連 `foo` 都算合法，拿它當刪除門檻等於把任何人放進 `tasks/` 的
/// 目錄都納入清理範圍。寧可漏掉幾個手工目錄，也不要讓一個不是本工具生成的
/// 名字走進刪除路徑。
fn is_generated_task_dirname(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 21
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'Z'
        && b[16] == b'-'
        && b[17..]
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
}

/// cmd_gc:1683 — 清掉夠舊的終態 task。未完成的、evict 收尾筆記（pinned）、
/// 以及「disposable 宣告已失效」的證據一律保留。
///
/// 保留證據那條（`kept_proof`）不是可有可無的保守：`idle` 判斷一個 disposable
/// 宣告是否被後續任務推翻，靠的就是掃 `tasks/` 找晚於 `disposable_at` 的任務；
/// 那個任務一旦被清掉，宣告會從 expired 復活成 yes，orchestrator 據此直接
/// 回收一個其實已有新脈絡的 worker。
///
/// 架構對映表把 `cmd_gc` 列在 `spawn` 模組（它與 disposable 宣告耦合），
/// 但 `spawn` 要到 M3 才建，而 gc 動的全部是 task 目錄；M1 先落在 `task`，
/// 對映表已同步更新（architecture.md §2）。
pub fn gc(paths: &Paths, days: u64, apply: bool, include_notes: bool) -> Result<GcStats> {
    let mut stats = GcStats::default();
    let cutoff = crate::time::now_epoch() - (days as i64) * 86400;

    // 現存 registry 裡還掛著 disposable 宣告的 agent → 宣告時間戳
    let mut pledge: Vec<(String, String)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&paths.agents_dir) {
        for entry in rd.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().map(|e| e != "json").unwrap_or(true) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(Value::Object(fields)) = json::parse(&content) else {
                continue;
            };
            if !json::bool_field_is_true(&fields, "disposable") {
                continue;
            }
            let name = json::str_field(&fields, "name").unwrap_or_default();
            let at = json::str_field(&fields, "disposable_at").unwrap_or_default();
            if !name.is_empty() && !at.is_empty() {
                pledge.push((name.to_string(), at.to_string()));
            }
        }
    }

    let mut dirs: Vec<std::path::PathBuf> = match std::fs::read_dir(&paths.tasks_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => return Ok(stats),
    };
    dirs.sort();

    for d in dirs {
        let id = match d.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !is_generated_task_dirname(&id) {
            continue;
        }
        if !d.join("metadata.json").is_file() || !d.join("status").is_file() {
            continue;
        }
        let Ok(st) = read_status(&d) else {
            continue;
        };
        if !matches!(st.as_str(), "completed" | "failed" | "cancelled") {
            stats.kept_live += 1;
            continue;
        }

        // metadata 讀不出來時一律偏保守：pinned 當 true、時間當「判不出年紀」，
        // 對映 bash `|| pinned=true` 與 `|| ts=""` 的失效方向。
        let meta = std::fs::read_to_string(d.join("metadata.json")).ok();
        let fields = meta.as_deref().and_then(|c| match json::parse(c) {
            Ok(Value::Object(m)) => Some(m),
            _ => None,
        });
        if !include_notes {
            let pinned = match &fields {
                Some(m) => json::bool_field_is_true(m, "pinned"),
                None => true,
            };
            if pinned {
                stats.kept_pin += 1;
                continue;
            }
        }

        let to_who = fields
            .as_ref()
            .and_then(|m| json::str_field(m, "to"))
            .unwrap_or_default()
            .to_string();
        // 用 metadata 的 created_at 而不是目錄 mtime：mtime 會被備份、rsync、
        // 檔案系統操作改掉，created_at 是這個任務自己的事實。
        let ts = fields
            .as_ref()
            .and_then(|m| json::str_field(m, "created_at"))
            .unwrap_or_default()
            .to_string();

        // 是某個現存宣告的「已失效證據」就留著。ISO 8601 UTC 定長字串可直接
        // 字典序比大小；`!(ts < pledge_at)` ＝同秒也算證據（bash:1751 的
        // 「不小於」，理由同 disposable_effective 的同秒判定）。
        if !to_who.is_empty()
            && let Some((_, at)) = pledge.iter().find(|(n, _)| *n == to_who)
            && !ts.is_empty()
            && ts.as_str() >= at.as_str()
        {
            stats.kept_proof += 1;
            continue;
        }

        // 讀不到時間就當它還年輕：判不出年紀的東西不刪
        let Some(epoch) = crate::time::parse_iso_to_epoch(&ts) else {
            stats.kept_young += 1;
            continue;
        };
        if epoch >= cutoff {
            stats.kept_young += 1;
            continue;
        }

        if !apply {
            stats.candidates.push((id.clone(), st.clone(), ts));
            stats.removed += 1;
            continue;
        }

        // 取這個 task 的鎖再刪：狀態轉換都在同一把鎖下，不取就可能在 reply
        // 寫到一半時把目錄抽走。取不到鎖只算這一個失敗，**不中止整輪**
        // （bash 用 subshell 達到同樣效果）。
        let Ok(guard) = crate::lock::acquire_lock(paths, &id) else {
            stats.failed += 1;
            continue;
        };
        // 拿到鎖後重驗狀態：等鎖那段時間裡它可能已經被 receive／reply 動過
        let still_terminal = read_status(&d)
            .map(|s| matches!(s.as_str(), "completed" | "failed" | "cancelled"))
            .unwrap_or(false);
        if !still_terminal {
            guard.release();
            stats.kept_live += 1;
            continue;
        }
        let removed = std::fs::remove_dir_all(&d).is_ok();
        guard.release();
        if removed {
            stats.removed += 1;
        } else {
            stats.failed += 1;
        }
    }
    Ok(stats)
}

/// `cancel` 一次執行的終態（CLI-CANCEL-1）。轉態本身若失敗會以 `Err` 收場，
/// 故拿到 `CancelOutcome` 即代表 task 已轉 `cancelled`；`notify` 只描述通知
/// 這一段（`None`＝對方未註冊，依 bash 正本不通知也不算失敗）。
#[derive(Clone, Debug)]
pub struct CancelOutcome {
    pub to: String,
    pub pane: String,
    /// 請對方執行的命令原文（通知失敗時要求人工執行的那條）
    pub cmdline: String,
    pub notify: Option<crate::notify::NotifyOutcome>,
}

/// cmd_cancel:744 的完整語意（鎖／轉態／事件／通知），CLI 與 TUI 的**單一
/// 正本**——先前 TUI 另抄一份，兩邊會各自漂移（審查 F6）。
///
/// **函式內不印任何字**：呈現層由呼叫端決定（CLI 印 stderr；TUI 進 footer，
/// alternate screen 下 stderr 會畫花畫面，審查 F7）。
pub fn cancel_task(
    paths: &Paths,
    tmux: &dyn crate::tmux::TmuxClient,
    id: &str,
) -> Result<CancelOutcome> {
    check_task_id(id)?;
    let dir = require_task_dir(paths, id)?;

    let guard = crate::lock::acquire_lock(paths, id)?;
    let outcome = (|| -> Result<()> {
        let st = read_status(&dir)?;
        if !matches!(st.as_str(), "queued" | "delivered" | "running") {
            return Err(Error::new(format!(
                "task 狀態為 {st}，無法 cancel（僅 queued/delivered/running 可）"
            )));
        }
        update_meta_status(&dir, TaskState::Cancelled)?;
        log_event(paths, id, "cancelled", "")
    })();
    guard.release();
    outcome?;

    // 通知 worker 查狀態（會看到 cancelled）；worker 未註冊就只是不通知
    let to = meta_str(&dir, "to")?;
    let cmdline = format!("agent-bridge status {id}");
    let agent_file = paths.agents_dir.join(format!("{to}.json"));
    let mut out = CancelOutcome {
        to,
        pane: String::new(),
        cmdline,
        notify: None,
    };
    if agent_file.is_file() {
        out.pane = crate::registry::read_pane(&agent_file);
        out.notify = Some(crate::notify::notify_or_defer_outcome(
            paths,
            tmux,
            &out.to,
            &out.pane,
            &out.cmdline,
            id,
            "status",
        )?);
    }
    Ok(out)
}

/// TUI read model 的一列（tui-design.md §4）：一個尚未到終態的任務。
/// `status` 是權威狀態字（`queued`／`delivered`／`running`）。
#[derive(Debug)]
pub struct InFlight {
    pub id: String,
    pub from: String,
    pub to: String,
    pub status: String,
}

/// in-flight（非終態）任務的唯讀快照（TUI read model，tui-design.md §4）。
///
/// 只認本工具生成形狀的目錄名與完整形狀（metadata＋status 都在）——與 gc
/// 的掃描門檻同一條理由：`tasks/` 裡任何人都能放目錄。單一損壞任務跳過
/// 而不是讓整份 dashboard 消失（同 `last_task_at` 的 `|| continue` 方向）。
/// 唯讀且**不取鎖**：dashboard 每 500ms 掃一輪，取鎖會與正常狀態轉換互撞；
/// 讀到轉換中途的舊值只是晚一輪刷新，不影響任何狀態機不變量。
pub fn in_flight(paths: &Paths) -> Vec<InFlight> {
    let Ok(rd) = std::fs::read_dir(&paths.tasks_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.filter_map(|e| e.ok()) {
        let dir = entry.path();
        let id = match dir.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !is_generated_task_dirname(&id) || !dir.join("metadata.json").is_file() {
            continue;
        }
        let Ok(st) = read_status(&dir) else { continue };
        if !matches!(st.as_str(), "queued" | "delivered" | "running") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(dir.join("metadata.json")) else {
            continue;
        };
        let Ok(Value::Object(fields)) = json::parse(&content) else {
            continue;
        };
        out.push(InFlight {
            id,
            from: json::jq_raw_field(&fields, "from").unwrap_or_default(),
            to: json::jq_raw_field(&fields, "to").unwrap_or_default(),
            status: st,
        });
    }
    // task-id 以時間戳起首，字典序＝時間序，畫面因此穩定
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// 近期任務（**含終態**）的唯讀快照，供 TUI 的 TASKS 面板使用
/// （tui-design.md §2 版面）。`in_flight` 只看得到非終態，而 `read` 只對
/// `completed`／`failed` 合法——沒有這一份，畫面上就沒有可讀的任務。
///
/// 紀律同 `in_flight`：唯讀、**不取鎖**、只認本工具生成形狀的目錄名、單一
/// 損壞任務跳過而不是讓整份 dashboard 消失。
///
/// **先按目錄名反序排序再截到 `limit`，之後才讀 status／metadata**：`tasks/`
/// 會長大，500ms 輪詢不能對整個目錄做 N 次檔案讀（截斷後至多 `limit` 筆 I/O）。
/// 目錄名以時間戳起首，字典序＝時間序，故反序＝新的在前。
pub fn recent_tasks(paths: &Paths, limit: usize) -> Vec<InFlight> {
    let Ok(rd) = std::fs::read_dir(&paths.tasks_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .filter(|n| is_generated_task_dirname(n))
        .collect();
    names.sort_by(|a, b| b.cmp(a));
    names.truncate(limit);

    let mut out = Vec::new();
    for id in names {
        let dir = paths.tasks_dir.join(&id);
        let Ok(st) = read_status(&dir) else { continue };
        // 非權威狀態字＝損壞，比照損壞目錄跳過。放行的話畫面的 status 軸就會
        // 出現不存在的 task 狀態（§2 硬條款），且未知值會被當成非終態而開得了
        // cancel 確認框
        if TaskState::parse(&st).is_none() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(dir.join("metadata.json")) else {
            continue;
        };
        let Ok(Value::Object(fields)) = json::parse(&content) else {
            continue;
        };
        out.push(InFlight {
            id,
            from: json::jq_raw_field(&fields, "from").unwrap_or_default(),
            to: json::jq_raw_field(&fields, "to").unwrap_or_default(),
            status: st,
        });
    }
    out
}

/// `read` 一次執行的終態：標頭欄位＋response 的**原始位元組**
/// （payload 邊界走 bytes 是架構 §3 的紅線，不經 `String` 中轉）。
#[derive(Debug)]
pub struct ReadOutcome {
    pub from: String,
    pub to: String,
    pub bytes: Vec<u8>,
}

/// `read` 的標頭欄位（取自 metadata，於鎖內讀出）。
pub struct ReadHeader {
    pub from: String,
    pub to: String,
}

/// cmd_read:784 的完整語意（鎖／狀態驗證／read 事件／payload 呈現），CLI 與
/// TUI 的**單一正本**——分家就會漂移（審查 F6，同 `cancel_task` 的處置）。
///
/// **持鎖到呼叫端把 payload 處理完為止**，故走 callback 而不是「先讀成
/// `Vec<u8>` 再回傳」：
/// - gc --apply 刪目錄前取的就是這把鎖，不取的話從驗完狀態到讀 response
///   之間目錄可被整個抽走；
/// - 更關鍵的是**呈現順序**。CLI 舊行為是「先印三行標頭到 stderr，再串流
///   payload 到 stdout」，兩者都在鎖內。若改成外殼拿到完整 outcome 才印，
///   `response.md` 缺檔時就變成一行標頭都不印——那是可觀察的行為改變
///   （跨廠審查 major #1）。
///
/// `read` 事件記在**呼叫 callback 之前**（順序同 bash 正本）。
/// **函式內不印任何字**：標頭與 payload 都交給 callback 呈現。
pub fn with_response<T>(
    paths: &Paths,
    id: &str,
    f: impl FnOnce(&ReadHeader, &Path) -> Result<T>,
) -> Result<T> {
    check_task_id(id)?;
    let dir = require_task_dir(paths, id)?;

    let guard = crate::lock::acquire_lock(paths, id)?;
    let outcome = (|| -> Result<T> {
        match read_status(&dir)?.as_str() {
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
        let header = ReadHeader {
            from: meta_str(&dir, "from")?,
            to: meta_str(&dir, "to")?,
        };
        log_event(paths, id, "read", "")?;
        f(&header, &dir.join("response.md"))
    })();
    guard.release();
    outcome
}

/// `with_response` 的 bytes 版（TUI 用：overlay pager 要的是完整位元組，
/// 沒有串流對象）。CLI **不走這條**——它要的是「先標頭再串流」的順序。
pub fn read_response(paths: &Paths, id: &str) -> Result<ReadOutcome> {
    with_response(paths, id, |h, path| {
        let bytes = std::fs::read(path)
            .map_err(|e| Error::new(format!("無法讀取 {}：{e}", path.display())))?;
        Ok(ReadOutcome {
            from: h.from.clone(),
            to: h.to.clone(),
            bytes,
        })
    })
}

/// await 的兩種正常終局。**逾時必須與操作性失敗分得開**：呼叫端（evict）只在
/// 真逾時才走「筆記沒落地仍回收」，其他非零是 await 自己壞掉——worker 可能還
/// 活著、根本沒等到期限，這時回收等於把活的 context 當逾時殺掉。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AwaitOutcome {
    Terminal(String),
    Timeout(String),
}

/// cmd_await:822 的等待主體（唯讀輪詢 status 檔，不寫 events、不取鎖）。
/// CLI 的 `await` 與 evict 的第二段共用同一份（審查 F6：分家就會漂移）。
///
/// **函式內不印任何字**：逾時的呈現（CLI 印一行再 exit 124）由呼叫端決定。
pub fn await_task(paths: &Paths, id: &str, timeout: u64) -> Result<AwaitOutcome> {
    let dir = require_task_dir(paths, id)?;

    // 輪詢間隔在進迴圈前就驗：壞值在 bash 會讓 sleep 立刻報錯、await 毫秒級
    // 非零退出，呼叫端若把這種操作性失敗當成逾時（evict 曾如此）就會殺掉還
    // 活著的 worker。此處同樣先驗後跑，維持「124 只等於真逾時」的契約。
    //
    // **`var_os` 而非 `var`**（codex 複核 2026-07-31 blocker）：`var()` 把
    // 「已設定但非 UTF-8」壓成 `Err`→空字串→退預設 1.0，而 bash 拿到的是原始
    // 位元組、regex 判不過就 die。差異不只多睡一秒——evict 會把這種 config
    // 錯誤誤分類成真逾時而去 despawn 一個還活著的 worker。非 UTF-8 在這裡
    // 走「值不合法」那條（訊息裡的值以 lossy 呈現，目的是讓人看見自己設了什麼）。
    let raw_os = std::env::var_os(crate::config::ENV_POLL_INTERVAL).unwrap_or_default();
    let raw = raw_os.to_string_lossy().into_owned();
    let is_unicode = raw_os.to_str().is_some();
    let interval: f64 = if !is_unicode {
        return Err(Error::new(format!(
            "AGENT_BRIDGE_POLL_INTERVAL 需為正數（秒）：{raw}"
        )));
    } else if raw.is_empty() {
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
        let st = read_status(&dir).map_err(|_| {
            Error::new(format!(
                "await 無法讀取 task {id} 的 status 檔（任務目錄被移走？）"
            ))
        })?;
        if matches!(st.as_str(), "completed" | "failed" | "cancelled") {
            return Ok(AwaitOutcome::Terminal(st));
        }
        if timeout > 0 && started.elapsed().as_secs() >= timeout {
            return Ok(AwaitOutcome::Timeout(st));
        }
        std::thread::sleep(poll_sleep(interval));
    }
}

/// 輪詢間隔（秒）→ `Duration`。
///
/// **`try_from_secs_f64` 而非 `from_secs_f64`**：後者對超出 `Duration` 值域的
/// finite 值是 **panic**，而 `await_task` 的驗證只擋掉「非正數／非有限」
/// ——`AGENT_BRIDGE_POLL_INTERVAL=1e300` 這種形狀會一路走到這裡。在 CLI 上
/// 那是 exit 101；在 TUI 的一次性 thread 上是工人直接 unwind、evict 的終局
/// 訊息永遠不會回來、in-flight 閘永遠不放開（codex 複核 major #3）。
/// bash 那邊是把值交給 `sleep`，超大值就睡到天荒地老；`Duration::MAX` 是同一
/// 個終態（處置同 `spawn::wait_ready`）。
fn poll_sleep(interval: f64) -> std::time::Duration {
    std::time::Duration::try_from_secs_f64(interval).unwrap_or(std::time::Duration::MAX)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 極大但 finite 的輪詢間隔 MUST NOT panic（`from_secs_f64` 會）：
    /// 那條路徑在 TUI 的一次性工人上會讓 evict 的終局訊息永遠不回來
    /// （codex 複核 major #3）。上界一律夾到 `Duration::MAX`（＝bash 的
    /// 「sleep 到天荒地老」）。
    #[test]
    fn poll_sleep_clamps_instead_of_panicking() {
        assert_eq!(poll_sleep(0.5), std::time::Duration::from_millis(500));
        assert_eq!(poll_sleep(2.0), std::time::Duration::from_secs(2));
        // `AGENT_BRIDGE_POLL_INTERVAL=1e300`：形狀合法（正、有限），值域外
        for huge in [1e300_f64, f64::MAX, 1.9e19] {
            assert_eq!(poll_sleep(huge), std::time::Duration::MAX, "值：{huge}");
        }
    }

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

    struct Dir {
        path: PathBuf,
    }

    impl Dir {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "{prefix}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Dir { path }
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn test_paths(d: &Dir) -> Paths {
        let paths = Paths {
            data_dir: d.path.clone(),
            agents_dir: d.path.join("agents"),
            tasks_dir: d.path.join("tasks"),
            locks_dir: d.path.join("locks"),
            state_dir: d.path.join("state"),
        };
        paths.ensure_dirs().unwrap();
        paths
    }

    #[test]
    fn create_task_writes_complete_shape() {
        let d = Dir::new("ab-core-task-test");
        let paths = test_paths(&d);
        let id = create_task(
            &paths,
            "alice",
            "bob",
            &MessageSource::Text("hello".into()),
            false,
        )
        .unwrap();
        let dir = task_dir(&paths, &id);
        assert_eq!(std::fs::read(dir.join("request.md")).unwrap(), b"hello\n");
        assert_eq!(read_status(&dir).unwrap(), "queued");
        assert_eq!(meta_str(&dir, "from").unwrap(), "alice");
        assert_eq!(meta_str(&dir, "to").unwrap(), "bob");
        // events.log 的 created 事件
        let log = std::fs::read_to_string(dir.join("events.log")).unwrap();
        assert!(log.contains("created from=alice to=bob"), "實際：{log}");
    }

    /// 訊息來源檔在預檢後消失時，MUST NOT 留下沒有 metadata/status 的殘缺
    /// 目錄（send_rollback:195-207 的孤兒目錄問題）。
    #[test]
    fn failed_create_rolls_back_partial_dir() {
        let d = Dir::new("ab-core-task-rb");
        let paths = test_paths(&d);
        let missing = d.path.join("no-such-message-file");
        let err =
            create_task(&paths, "alice", "bob", &MessageSource::File(missing), false).unwrap_err();
        assert!(
            err.message.contains("殘缺目錄已回滾"),
            "實際：{}",
            err.message
        );
        let leftovers: Vec<_> = std::fs::read_dir(&paths.tasks_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(leftovers.is_empty(), "殘缺目錄未清除：{leftovers:?}");
    }

    /// **先寫裸 status、再寫 metadata**（分組 31i 鎖的 split-brain 方向）。
    /// 套件那條斷言是源碼耦合的（`sed` 抽 bash 函式本體比對兩行的行號），
    /// 抽取對象在 M3 固定成 bash 正本，Rust 這側改由本測試守。
    ///
    /// 觀察法：把 `status` 換成目錄，讓那次寫入必然失敗（rename 覆蓋不了
    /// 目錄）。順序正確的話 metadata 根本還沒被動，`status` 欄位維持舊值；
    /// 順序若被對調，metadata 會先落地成新狀態——那正是「終態轉換可被重放」
    /// 的殘留方向。
    #[test]
    fn status_is_written_before_metadata() {
        let d = Dir::new("ab-core-task-wr-order");
        let paths = test_paths(&d);
        let id = create_task(&paths, "a", "b", &MessageSource::Text(b"x".to_vec()), false).unwrap();
        let dir = task_dir(&paths, &id);

        std::fs::remove_file(dir.join("status")).unwrap();
        std::fs::create_dir(dir.join("status")).unwrap();

        assert!(
            update_meta_status(&dir, TaskState::Completed).is_err(),
            "status 寫入必須失敗，測試前提才成立"
        );
        assert_eq!(
            meta_str(&dir, "status").unwrap(),
            "queued",
            "status 寫入失敗時 metadata 不得已經翻成終態（寫入順序被對調）"
        );
    }

    /// update_meta_status MUST 保持 metadata 既有欄位序（jq 賦值語意），
    /// 且裸 status 與 metadata.status 同步。
    #[test]
    fn status_update_keeps_field_order() {
        let d = Dir::new("ab-core-task-order");
        let paths = test_paths(&d);
        let id = create_task(&paths, "a", "b", &MessageSource::Text("x".into()), false).unwrap();
        let dir = task_dir(&paths, &id);
        update_meta_status(&dir, TaskState::Delivered).unwrap();
        assert_eq!(read_status(&dir).unwrap(), "delivered");
        let meta = std::fs::read_to_string(dir.join("metadata.json")).unwrap();
        let keys: Vec<&str> = meta
            .lines()
            .filter_map(|l| l.trim().split('"').nth(1))
            .collect();
        assert_eq!(
            keys,
            vec![
                "version",
                "task_id",
                "from",
                "to",
                "created_at",
                "updated_at",
                "working_directory",
                "status"
            ]
        );
        assert!(meta.contains("\"status\": \"delivered\""), "實際：{meta}");
        assert!(meta.starts_with("{\n  \"version\": 1,\n"), "實際：{meta}");
    }

    /// 缺欄位／null 一律回字面 `"null"`（jq -r 的輸出），標頭與 sender 查找
    /// 兩邊才不會與 bash 分岔；metadata 不是 object 則整體失敗。
    #[test]
    fn meta_str_matches_jq_raw_output() {
        let d = Dir::new("ab-core-task-meta");
        let paths = test_paths(&d);
        let id = create_task(&paths, "a", "b", &MessageSource::Text("x".into()), false).unwrap();
        let dir = task_dir(&paths, &id);
        assert_eq!(meta_str(&dir, "from").unwrap(), "a");
        assert_eq!(meta_str(&dir, "no_such_field").unwrap(), "null");

        std::fs::write(dir.join("metadata.json"), "{\"from\": null}\n").unwrap();
        assert_eq!(meta_str(&dir, "from").unwrap(), "null");

        std::fs::write(dir.join("metadata.json"), "[1,2]\n").unwrap();
        assert!(meta_str(&dir, "from").is_err(), "非 object 必須整體失敗");
    }

    /// pinned 任務的 metadata 多一個尾欄位（evict 收尾筆記，gc 一律跳過）。
    #[test]
    fn pinned_task_metadata_has_pinned_flag() {
        let d = Dir::new("ab-core-task-pin");
        let paths = test_paths(&d);
        let id = create_task(&paths, "a", "b", &MessageSource::Text("x".into()), true).unwrap();
        let meta = std::fs::read_to_string(task_dir(&paths, &id).join("metadata.json")).unwrap();
        assert!(meta.contains("\"pinned\": true"), "實際：{meta}");
    }

    /// gc 的黑箱 parity 分組（28）含 28e 的 spawn/evict 呼叫，gate 延至 M3；
    /// M1 期由以下單元測試護住核心判定（計畫 M1 列明文）。
    mod gc_tests {
        use super::*;

        /// 造一個指定 id／狀態／建立時間的完整 task 目錄。
        fn make_task(paths: &Paths, id: &str, status: &str, created_at: &str, extra: &str) {
            let dir = paths.tasks_dir.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("status"), format!("{status}\n")).unwrap();
            std::fs::write(
                dir.join("metadata.json"),
                format!(
                    "{{\n  \"version\": 1,\n  \"task_id\": \"{id}\",\n  \"from\": \"alice\",\n  \"to\": \"bob\",\n  \"created_at\": \"{created_at}\",\n  \"updated_at\": \"{created_at}\",\n  \"working_directory\": \"/tmp\",\n  \"status\": \"{status}\"{extra}\n}}\n"
                ),
            )
            .unwrap();
        }

        const OLD: &str = "2020-01-01T00:00:00Z";
        const OLD_ID: &str = "20200101T000000Z-aaaa";

        #[test]
        fn old_terminal_task_is_listed_then_removed() {
            let d = Dir::new("ab-core-gc-basic");
            let paths = test_paths(&d);
            make_task(&paths, OLD_ID, "completed", OLD, "");

            // 試算：列出候選、不刪
            let s = gc(&paths, 14, false, false).unwrap();
            assert_eq!(s.removed, 1);
            assert_eq!(
                s.candidates,
                vec![(OLD_ID.to_string(), "completed".to_string(), OLD.to_string())]
            );
            assert!(paths.tasks_dir.join(OLD_ID).is_dir(), "試算不得真的刪除");

            // --apply：真刪
            let s = gc(&paths, 14, true, false).unwrap();
            assert_eq!(s.removed, 1);
            assert_eq!(s.failed, 0);
            assert!(!paths.tasks_dir.join(OLD_ID).exists());
        }

        /// 未完成的一律保留，不論多舊。
        #[test]
        fn unfinished_task_is_kept() {
            let d = Dir::new("ab-core-gc-live");
            let paths = test_paths(&d);
            make_task(&paths, OLD_ID, "queued", OLD, "");
            let s = gc(&paths, 14, true, false).unwrap();
            assert_eq!((s.removed, s.kept_live), (0, 1));
            assert!(paths.tasks_dir.join(OLD_ID).is_dir());
        }

        /// evict 收尾筆記（pinned）預設保留；`--include-notes` 才納入清理。
        #[test]
        fn pinned_note_kept_unless_include_notes() {
            let d = Dir::new("ab-core-gc-pin");
            let paths = test_paths(&d);
            make_task(&paths, OLD_ID, "completed", OLD, ",\n  \"pinned\": true");
            let s = gc(&paths, 14, true, false).unwrap();
            assert_eq!((s.removed, s.kept_pin), (0, 1));
            let s = gc(&paths, 14, true, true).unwrap();
            assert_eq!(s.removed, 1);
        }

        /// 「disposable 宣告已失效」的證據不能刪：晚於（含同秒）宣告時間的
        /// 任務一旦消失，idle 會把 expired 的宣告判回 yes，orchestrator 據此
        /// 回收一個其實已有新脈絡的 worker。
        #[test]
        fn evidence_of_expired_pledge_is_kept() {
            let d = Dir::new("ab-core-gc-proof");
            let paths = test_paths(&d);
            std::fs::write(
                paths.agents_dir.join("bob.json"),
                "{\n  \"name\": \"bob\",\n  \"pane_id\": \"%1\",\n  \"disposable\": true,\n  \"disposable_at\": \"2019-01-01T00:00:00Z\"\n}\n",
            )
            .unwrap();
            make_task(&paths, OLD_ID, "completed", OLD, "");
            let s = gc(&paths, 14, true, false).unwrap();
            assert_eq!((s.removed, s.kept_proof), (0, 1));
            assert!(paths.tasks_dir.join(OLD_ID).is_dir());

            // 宣告晚於任務時，該任務不再是證據，照常清理
            std::fs::write(
                paths.agents_dir.join("bob.json"),
                "{\n  \"name\": \"bob\",\n  \"pane_id\": \"%1\",\n  \"disposable\": true,\n  \"disposable_at\": \"2021-01-01T00:00:00Z\"\n}\n",
            )
            .unwrap();
            let s = gc(&paths, 14, true, false).unwrap();
            assert_eq!((s.removed, s.kept_proof), (1, 0));
        }

        /// 未滿天數、以及讀不出建立時間的（判不出年紀）一律保留。
        #[test]
        fn young_and_undatable_tasks_are_kept() {
            let d = Dir::new("ab-core-gc-young");
            let paths = test_paths(&d);
            let id =
                create_task(&paths, "a", "bob", &MessageSource::Text("x".into()), false).unwrap();
            update_meta_status(&task_dir(&paths, &id), TaskState::Completed).unwrap();
            // 判不出年紀者：metadata 損壞（pinned 亦判不出，故先落在 kept_pin）
            let bad = "20200101T000000Z-bbbb";
            make_task(&paths, bad, "completed", OLD, "");
            std::fs::write(
                paths.tasks_dir.join(bad).join("metadata.json"),
                "{ not json",
            )
            .unwrap();

            let s = gc(&paths, 14, true, false).unwrap();
            assert_eq!(s.removed, 0);
            assert_eq!(s.kept_young, 1, "剛建立的終態任務應算年輕");
            assert_eq!(s.kept_pin, 1, "metadata 損壞時 pinned 偏保守判 true");
            assert!(paths.tasks_dir.join(bad).is_dir());
        }

        /// 曆法上不存在的 created_at（`2026-02-31`）＝判不出年紀，MUST 保留。
        /// bash 走 `date -ud`（對這種輸入 rc=1）落在 kept_young 分支；Rust 的
        /// 時戳解析若靜默換算，這個目錄反而會被刪。
        #[test]
        fn invalid_calendar_timestamp_keeps_task() {
            let d = Dir::new("ab-core-gc-badcal");
            let paths = test_paths(&d);
            make_task(&paths, OLD_ID, "completed", "2026-02-31T00:00:00Z", "");
            let s = gc(&paths, 14, true, false).unwrap();
            assert_eq!((s.removed, s.kept_young), (0, 1));
            assert!(paths.tasks_dir.join(OLD_ID).is_dir());
        }

        /// **非本工具生成的目錄名一律不碰**：TASK_ID_RE 連 `foo` 都算合法，
        /// 拿它當刪除門檻等於把任何人放進 tasks/ 的目錄納入清理範圍。
        #[test]
        fn foreign_directory_names_are_never_touched() {
            let d = Dir::new("ab-core-gc-foreign");
            let paths = test_paths(&d);
            for name in ["foo", "important-notes", "20200101T000000Z-XYZW", "2020"] {
                make_task(&paths, name, "completed", OLD, "");
            }
            let s = gc(&paths, 14, true, false).unwrap();
            assert_eq!(s, GcStats::default(), "外來目錄不得計入任何分類");
            for name in ["foo", "important-notes", "20200101T000000Z-XYZW", "2020"] {
                assert!(paths.tasks_dir.join(name).is_dir(), "{name} 被動到了");
            }
        }
    }

    /// `cancel_task` 是 CLI 與 TUI 的單一正本（審查 F6）：轉態＋`cancelled`
    /// 事件＋通知終態一次到位，且**函式內不印字**（F7，呈現交外殼）。
    #[test]
    fn cancel_task_transitions_and_reports_notify_outcome() {
        struct FakeTmux {
            sent: std::cell::RefCell<Vec<String>>,
        }
        impl crate::tmux::TmuxClient for FakeTmux {
            fn exec(&self, _args: &[&str]) -> Option<crate::tmux::TmuxOutput> {
                None
            }
            fn available(&self) -> bool {
                true
            }
            fn resolve_pane(&self, _t: &str) -> Option<String> {
                None
            }
            fn pane_exists(&self, _p: &str) -> bool {
                true
            }
            fn capture_pane(&self, _p: &str) -> Option<String> {
                Some(String::new())
            }
            fn pane_in_mode(&self, _p: &str) -> Option<bool> {
                Some(false)
            }
            fn send_keys(&self, _p: &str, keys: &str) -> bool {
                self.sent.borrow_mut().push(keys.to_string());
                true
            }
        }

        let d = Dir::new("ab-core-task-cancel");
        let paths = test_paths(&d);
        let id = create_task(
            &paths,
            "alice",
            "bob",
            &MessageSource::Text("x".into()),
            false,
        )
        .unwrap();
        std::fs::write(
            paths.agents_dir.join("bob.json"),
            "{\"name\":\"bob\",\"pane_id\":\"%7\"}\n",
        )
        .unwrap();

        let tmux = FakeTmux {
            sent: std::cell::RefCell::new(Vec::new()),
        };
        let out = cancel_task(&paths, &tmux, &id).unwrap();
        assert_eq!(read_status(&task_dir(&paths, &id)).unwrap(), "cancelled");
        assert_eq!((out.to.as_str(), out.pane.as_str()), ("bob", "%7"));
        assert_eq!(out.cmdline, format!("agent-bridge status {id}"));
        assert_eq!(out.notify, Some(crate::notify::NotifyOutcome::Notified));
        let events = std::fs::read_to_string(paths.tasks_dir.join(&id).join("events.log")).unwrap();
        assert!(events.contains("cancelled"), "實際：{events}");
        assert!(events.contains("notified"), "通知事件必須落地：{events}");
        assert!(
            tmux.sent.borrow().iter().any(|k| k.contains(&id)),
            "MUST 通知對方查狀態"
        );

        // 終態不可再取消（與 CLI 同一條轉換條件）
        let err = cancel_task(&paths, &tmux, &id).unwrap_err();
        assert!(err.message.contains("cancelled"), "實際：{}", err.message);
    }

    /// in_flight 只收非終態＋生成形狀＋完整形狀；損壞任務跳過不致整份消失。
    #[test]
    fn in_flight_filters_terminal_foreign_and_corrupt() {
        let d = Dir::new("ab-core-task-inflight");
        let paths = test_paths(&d);
        let q = create_task(
            &paths,
            "alice",
            "bob",
            &MessageSource::Text("x".into()),
            false,
        )
        .unwrap();
        let r = create_task(
            &paths,
            "alice",
            "bob",
            &MessageSource::Text("y".into()),
            false,
        )
        .unwrap();
        update_meta_status(&task_dir(&paths, &r), TaskState::Running).unwrap();
        let done = create_task(
            &paths,
            "alice",
            "bob",
            &MessageSource::Text("z".into()),
            false,
        )
        .unwrap();
        update_meta_status(&task_dir(&paths, &done), TaskState::Completed).unwrap();
        // 外來目錄名與損壞 status 各一：都不得出現，也不得毒死整份快照
        std::fs::create_dir(paths.tasks_dir.join("foreign-dir")).unwrap();
        let bad = paths.tasks_dir.join("20200101T000000Z-dead");
        std::fs::create_dir(&bad).unwrap();
        std::fs::write(bad.join("metadata.json"), "{ not json").unwrap();
        std::fs::write(bad.join("status"), "queued\n").unwrap();

        let rows = in_flight(&paths);
        let ids: Vec<&str> = rows.iter().map(|t| t.id.as_str()).collect();
        let mut expect = vec![q.as_str(), r.as_str()];
        expect.sort();
        assert_eq!(ids, expect, "只收非終態且形狀完整者，並依 id 排序");
        for t in &rows {
            assert_eq!((t.from.as_str(), t.to.as_str()), ("alice", "bob"));
            assert!(matches!(t.status.as_str(), "queued" | "running"));
        }
    }

    /// payload byte 保真：非 UTF-8 位元組原樣進出，不得 lossy 轉換（分組 6）。
    #[test]
    fn message_file_bytes_are_preserved() {
        let d = Dir::new("ab-core-task-bytes");
        let paths = test_paths(&d);
        let raw: Vec<u8> = vec![0xff, 0xfe, 0x00, 0x41, 0x0a, 0x80];
        let src_file = d.path.join("payload.bin");
        std::fs::write(&src_file, &raw).unwrap();
        let id = create_task(&paths, "a", "b", &MessageSource::File(src_file), false).unwrap();
        let got = std::fs::read(task_dir(&paths, &id).join("request.md")).unwrap();
        assert_eq!(got, raw);
    }

    /// 手鋪一個終態 task（不經 send／reply，狀態確定停在指定值）。
    fn seed_task(paths: &Paths, id: &str, status: &str, response: Option<&[u8]>) {
        let dir = paths.tasks_dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("status"), format!("{status}\n")).unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            format!(
                "{{\"version\":1,\"task_id\":\"{id}\",\"from\":\"alice\",\"to\":\"bob\",\"status\":\"{status}\"}}\n"
            ),
        )
        .unwrap();
        if let Some(b) = response {
            std::fs::write(dir.join("response.md"), b).unwrap();
        }
    }

    /// tui-design §9 P2 gate (b) 的正本：`read_response` 回的 bytes MUST **逐
    /// byte** 等於 `response.md`（非 ASCII、trailing newline、非文字 byte 全數
    /// 原樣），且呼叫後 events.log 多一筆 `read`（read 非唯讀路徑）。
    #[test]
    fn read_response_returns_verbatim_bytes_and_logs_read_event() {
        let d = Dir::new("ab-core-read-bytes");
        let paths = test_paths(&d);
        let id = "20260731T000010Z-abcd";
        let mut raw: Vec<u8> = "回覆內容 ✓\n".as_bytes().to_vec();
        raw.push(0x00); // 非文字 byte：證明走的是 bytes 不是字串
        raw.extend_from_slice(b"tail\n");
        seed_task(&paths, id, "completed", Some(&raw));

        let out = read_response(&paths, id).unwrap();
        assert_eq!(out.bytes, raw, "MUST 逐 byte 等於 response.md");
        assert_eq!((out.from.as_str(), out.to.as_str()), ("alice", "bob"));
        let log = std::fs::read_to_string(task_dir(&paths, id).join("events.log")).unwrap();
        assert_eq!(
            log.lines().filter(|l| l.ends_with(" read")).count(),
            1,
            "實際 events.log：{log}"
        );
    }

    /// 拒絕路徑的訊息逐字釘住（CLI-READ-1：cancelled 與未回覆兩條措辭不同，
    /// 未來改寫會被這裡抓到——CLI 的 stderr 就是這兩條字串）。
    #[test]
    fn read_response_rejects_non_readable_states_verbatim() {
        let d = Dir::new("ab-core-read-reject");
        let paths = test_paths(&d);
        let q = "20260731T000011Z-bbbb";
        let c = "20260731T000012Z-cccc";
        seed_task(&paths, q, "queued", None);
        seed_task(&paths, c, "cancelled", None);
        assert_eq!(
            read_response(&paths, q).unwrap_err().message,
            format!("task {q} 尚未回覆（狀態：queued）；查詢進度請用 agent-bridge status {q}")
        );
        assert_eq!(
            read_response(&paths, c).unwrap_err().message,
            format!("task {c} 已取消（cancelled），沒有回覆可讀")
        );
        // 拒絕路徑不得留下 read 事件
        assert!(!task_dir(&paths, q).join("events.log").exists());
    }

    /// `recent_tasks`：含終態、依 id 反序（新的在上）、limit 生效、
    /// 損壞目錄跳過而不是讓整份快照消失。
    #[test]
    fn recent_tasks_are_reverse_ordered_limited_and_corruption_tolerant() {
        let d = Dir::new("ab-core-recent");
        let paths = test_paths(&d);
        seed_task(&paths, "20260731T000001Z-0001", "completed", Some(b"x"));
        seed_task(&paths, "20260731T000002Z-0002", "queued", None);
        seed_task(&paths, "20260731T000003Z-0003", "cancelled", None);
        // 損壞（metadata 不是 JSON）與外來目錄名各一
        let bad = paths.tasks_dir.join("20260731T000004Z-0004");
        std::fs::create_dir(&bad).unwrap();
        std::fs::write(bad.join("status"), "queued\n").unwrap();
        std::fs::write(bad.join("metadata.json"), "{ not json").unwrap();
        std::fs::create_dir(paths.tasks_dir.join("foreign-dir")).unwrap();

        let rows = recent_tasks(&paths, 100);
        let ids: Vec<&str> = rows.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "20260731T000003Z-0003",
                "20260731T000002Z-0002",
                "20260731T000001Z-0001"
            ],
            "反序（新的在上）＋終態亦在列＋損壞者跳過"
        );
        assert_eq!(rows[0].status, "cancelled");
        // limit 在**讀檔前**截斷：最新的一筆正是那個損壞目錄，limit=1 時
        // 截斷後只剩它，讀不出東西就是空清單（證明截斷不是發生在讀檔之後）
        assert!(
            recent_tasks(&paths, 1).is_empty(),
            "截斷在排序之後、讀檔之前"
        );
        let two = recent_tasks(&paths, 2);
        assert_eq!(two.len(), 1);
        assert_eq!(two[0].id, "20260731T000003Z-0003");
    }

    /// 非權威狀態字（`tasks/` 裡任何人都能寫）MUST NOT 進 read model：
    /// 放行的話畫面的 status 軸就會出現不存在的 task 狀態，而設計正本 §2
    /// 明訂沒有 `blocked` 這個 task 狀態（跨廠審查 major #2）。
    #[test]
    fn recent_tasks_reject_non_authoritative_status() {
        let d = Dir::new("ab-core-recent-status");
        let paths = test_paths(&d);
        seed_task(&paths, "20260731T000001Z-000a", "completed", Some(b"x"));
        for (id, st) in [
            ("20260731T000002Z-000b", "blocked"),
            ("20260731T000003Z-000c", "COMPLETED"),
            ("20260731T000004Z-000d", ""),
        ] {
            seed_task(&paths, id, st, None);
        }

        let rows = recent_tasks(&paths, 100);
        let ids: Vec<&str> = rows.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["20260731T000001Z-000a"],
            "只有六個權威狀態字放行；blocked／大小寫變體／空字串一律跳過"
        );

        // 六個權威字逐一放行（避免未來收窄過頭把合法狀態也擋掉）
        for st in [
            "queued",
            "delivered",
            "running",
            "completed",
            "failed",
            "cancelled",
        ] {
            assert!(TaskState::parse(st).is_some(), "MUST 放行：{st}");
        }
        assert!(TaskState::parse("blocked").is_none());
    }
}
