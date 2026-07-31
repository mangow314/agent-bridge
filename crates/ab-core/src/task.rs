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

#[cfg(test)]
mod tests {
    use super::*;

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
}
