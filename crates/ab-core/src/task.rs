//! task 目錄結構、id 生成／驗證、狀態機轉換與 events.log（state.md STATE-TASK-*、
//! cli.md 轉換條款）。對映 bash `write_message`、`log_event`、
//! `update_meta_status`、`check_task_id`、`send_rollback`、
//! `cmd_send` 的 task 建立段。

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

/// check_task_id — 文法不合即 die，訊息逐字對齊。
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

/// 讀裸 status 檔。**裸 status 是操作上的權威**（update_meta_status 的
/// 寫入順序註記），狀態機與 await 全數讀它，不讀 metadata.status。
pub fn read_status(dir: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(dir.join("status"))
        .map_err(|e| Error::new(format!("無法讀取 status 檔：{e}")))?;
    Ok(raw.trim_end_matches('\n').to_string())
}

/// log_event — append-only events.log，行格式 `<iso> <event>[ <detail>]`。
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

/// update_meta_status — 同步更新 metadata.json 與裸 status 檔。
/// **寫入順序不可對調**：先裸 status、後 metadata（bash 的論證——中途
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

/// write_message — payload 走 byte 流原樣搬運（分組 6 驗 byte-for-byte
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
            // 第二道存在性檢查（cmd_send 的註記：預檢與此處之間的
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

/// `last_task_at` — 最後一個派給該 agent 的任務建立時間（ISO），從未被
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

/// rand_suffix — `head -c 2 /dev/urandom | od -An -tx1`：2 bytes 轉 4 位
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
/// 對映 bash 的 `SEND_RB_DIR`＋EXIT trap：中途任何失敗都
/// 要把殘缺目錄收走——沒有 metadata/status 的目錄不屬於任何狀態，
/// 留下就是永久孤兒。`commit()` 之後才解除。
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

/// cmd_send 的 task 建立段：碰撞重試建目錄 → 寫 request → 寫
/// metadata → 寫 status → 記 created 事件。回傳 task-id。
///
/// 這裡的回滾用 `Drop`（與架構 §6 對鎖的紅線不同）是有意的：鎖的紅線是
/// 「殘鎖行為必須與 bash 等價」，而 bash 的回滾本身就是 EXIT trap——同樣不
/// 在 SIGKILL 下執行。兩者的失效邊界一致，故此處 Drop 是等價實作而非弱化。
pub fn create_task(paths: &Paths, from: &str, to: &str, src: &MessageSource) -> Result<String> {
    let mut tries = 0;
    let (task_id, dir) = loop {
        let id = format!("{}-{}", now_compact(), rand_suffix());
        let d = paths.tasks_dir.join(&id);
        match std::fs::create_dir(&d) {
            Ok(()) => break (id, d),
            // 只有「已存在」才是 task-id 碰撞。權限不足、唯讀 fs、路徑不是目錄
            // 等一律立即返回並保留 cause：把它們磨成碰撞訊息會把人帶去錯的方向
            // 查，還白白重試五次。碰撞那句本身逐字不變（既有 CLI 面）。
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(Error::new(format!(
                    "無法建立 task 目錄 {}：{e}",
                    d.display()
                )));
            }
        }
        tries += 1;
        if tries >= 5 {
            return Err(Error::new("無法建立 task 目錄（task-id 連續碰撞）"));
        }
    };
    let rb = SendRollback {
        dir: Some(dir.clone()),
    };

    // die 文字**逐字**對齊 bash，不得附加底層錯誤原因：
    // 這三句是 CLI 契約面，尾巴多一個「：{io error}」就不是同一句話了。
    write_message(&dir.join("request.md"), src)
        .map_err(|_| Error::new("send 無法寫入 request（殘缺目錄已回滾）"))?;

    let ts = now_iso();
    let wd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // 欄位序逐字對齊 bash `jq -n '{version, task_id, from, to, created_at,
    // updated_at, working_directory, status}'`；
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
    let doc = format!("{}\n", json::render_pretty(&Value::Object(fields)));
    atomic_write(&dir.join("metadata.json"), doc.as_bytes())
        .map_err(|_| Error::new("send 無法寫入 metadata（殘缺目錄已回滾）"))?;
    atomic_write(&dir.join("status"), b"queued\n")
        .map_err(|_| Error::new("send 無法寫入 status（殘缺目錄已回滾）"))?;
    rb.commit();

    log_event(paths, &task_id, "created", &format!("from={from} to={to}"))?;
    Ok(task_id)
}

/// send 生成的 task 目錄名形狀：`^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{4}$`。
///
/// **這裡刻意不用寬鬆的 `TASK_ID_RE`**：公開驗證用的 TASK_ID_RE 連 `foo` 都
/// 算合法，而 `tasks/` 裡任何人都能放目錄；掃描只認本工具生成的名字。
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

/// cmd_cancel 的完整語意（鎖／轉態／事件／通知）的**單一正本**（審查 F6）。
///
/// **函式內不印任何字**：呈現層由呼叫端決定（CLI 印 stderr，審查 F7）。
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

/// in-flight read model 的一列（tag `pre-trim-2026-09` 的 docs/tui-design.md §4）：一個尚未到終態的任務。
/// `status` 是權威狀態字（`queued`／`delivered`／`running`）。
#[derive(Debug)]
pub struct InFlight {
    pub id: String,
    pub from: String,
    pub to: String,
    pub status: String,
    /// metadata 的 `created_at`（ISO 8601 UTC）。**這是記憶體 read model 的
    /// 欄位，不是磁碟 schema 的新欄位**——metadata 本來就有它，這裡只是把它
    /// 一起讀進來。缺失／讀不到＝空字串（呼叫端 fail-closed）。
    ///
    /// 為什麼歸屬判定要用它而不是 task-id 前綴：id 前綴只是 created_at
    /// 的衍生副本，兩者不保證一致（測試 fixture 就有一份不一致的）。同一份
    /// read model 對「這個 task 幾點生的」不該有兩套答案——一律以 metadata
    /// 的 `created_at` 為準，不看目錄 mtime。
    pub created_at: String,
}

/// in-flight（非終態）任務的唯讀快照（read model，tag `pre-trim-2026-09` 的 docs/tui-design.md §4）。
///
/// 只認本工具生成形狀的目錄名與完整形狀（metadata＋status 都在）：
/// `tasks/` 裡任何人都能放目錄。單一損壞任務跳過
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
            created_at: json::jq_raw_field(&fields, "created_at").unwrap_or_default(),
        });
    }
    // task-id 以時間戳起首，字典序＝時間序，畫面因此穩定
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// `read` 的標頭欄位（取自 metadata，於鎖內讀出）。
pub struct ReadHeader {
    pub from: String,
    pub to: String,
}

/// cmd_read 的完整語意（鎖／狀態驗證／read 事件／payload 呈現）的**單一
/// 正本**——分家就會漂移（審查 F6，同 `cancel_task` 的處置）。
///
/// **持鎖到呼叫端把 payload 處理完為止**，故走 callback 而不是「先讀成
/// `Vec<u8>` 再回傳」：
/// - 取 task 鎖，避免從驗完狀態到讀 response 之間目錄被整個抽走；
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

/// await 的正常終局。**逾時必須與操作性失敗分得開**：呼叫端（evict）只在
/// 真逾時才走「筆記沒落地仍回收」，其他非零是 await 自己壞掉——worker 可能還
/// 活著、根本沒等到期限，這時回收等於把活的 context 當逾時殺掉。
///
/// `Blocked` 只可能出自 `await_task_watched` 帶 `BlockerPolicy::Return`：
/// worker pane 持續停在權限確認框（去抖＋grace 都滿）。它**不是逾時**——
/// worker 還活著，只是在等一個人；呼叫端的正確處置是去 pane 前裁決或告警，
/// 不是回收（CLI-AWAIT-4）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AwaitOutcome {
    Terminal(String),
    Timeout(String),
    /// status＝返回當下的 task 狀態；since_secs＝權限框持續秒數（去抖確立起算）
    Blocked {
        status: String,
        since_secs: u64,
    },
}

/// blocker 偵測到時的行為（CLI `--on-blocker`；預設 Warn）。
///
/// 預設不是 Return 的理由：`screen_has_prompt` 有誤判史（收窄前 19/24），
/// warn 誤報的代價是一行 stderr，Return 誤報會讓協調者把工作中的 worker
/// 當成卡死去處置。無人值守派工的 await 應顯式帶 `--on-blocker return`
/// （orchestrator-brief 的派工紀律）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockerPolicy {
    Warn,
    Return,
}

/// `await_task_watched` 的探測設定。`on_warn(blocker, sustained_secs)` 由呼叫
/// 端呈現（core 不印字）；每種 blocker 至多喚一次，狀態清空後才會再喚。
pub struct BlockerWatch<'a> {
    pub tmux: &'a dyn crate::tmux::TmuxClient,
    pub pane: String,
    pub policy: BlockerPolicy,
    /// Prompt 去抖確立後，還要持續這麼多秒才觸發 Return（Warn 不受此限）
    pub grace_secs: u64,
    pub on_warn: &'a dyn Fn(crate::notify::PaneBlocker, u64),
}

/// 每幾輪 status 輪詢做一次 blocker 探測（攤薄 tmux 查詢成本；預設輪詢 1s
/// 間隔下約每 5 秒一次）。
const PROBE_EVERY: u32 = 5;
/// Prompt 須連續命中幾次探測才算確立（單次誤判不觸發任何行為）。
const PROBE_DEBOUNCE: u32 = 2;

/// 探測結果 → 行為的純狀態機（時間注入，單測不需睡眠）。
///
/// 語意（CLI-AWAIT-4）：Prompt 連續 `PROBE_DEBOUNCE` 次確立 → 喚 warn 一次；
/// 確立起算持續滿 grace → 報 blocked（是否據此返回是呼叫端的 policy）。
/// Clear/CopyMode＝確認非權限框 → 去抖與持續計時歸零、解除後允許再警告；
/// CopyMode 與 Gone 各至多警告一次；Unknown（查詢層失敗）**不歸零不警告**
/// ——查不到不是「已解除」的證據。
struct BlockerTracker {
    prompt_hits: u32,
    prompt_since: Option<std::time::Instant>,
    warned_prompt: bool,
    warned_copy: bool,
    warned_gone: bool,
}

/// 一次觀測產出的行為：warn 至多一個、blocked 至多一個（同次可並發：
/// 確立當下 grace=0 時警告與返回同時成立）。
struct BlockerActions {
    warn: Option<(crate::notify::PaneBlocker, u64)>,
    blocked_since: Option<u64>,
}

impl BlockerTracker {
    fn new() -> Self {
        BlockerTracker {
            prompt_hits: 0,
            prompt_since: None,
            warned_prompt: false,
            warned_copy: false,
            warned_gone: false,
        }
    }

    fn observe(
        &mut self,
        seen: crate::notify::PaneBlocker,
        policy: BlockerPolicy,
        grace_secs: u64,
        now: std::time::Instant,
    ) -> BlockerActions {
        use crate::notify::PaneBlocker as B;
        let mut act = BlockerActions {
            warn: None,
            blocked_since: None,
        };
        match seen {
            B::Prompt => {
                self.prompt_hits = self.prompt_hits.saturating_add(1);
                if self.prompt_hits >= PROBE_DEBOUNCE {
                    let since = *self.prompt_since.get_or_insert(now);
                    let sustained = now.saturating_duration_since(since).as_secs();
                    if !self.warned_prompt {
                        act.warn = Some((B::Prompt, sustained));
                        self.warned_prompt = true;
                    }
                    if policy == BlockerPolicy::Return && sustained >= grace_secs {
                        act.blocked_since = Some(sustained);
                    }
                }
            }
            B::Clear | B::CopyMode => {
                self.prompt_hits = 0;
                self.prompt_since = None;
                self.warned_prompt = false;
                if seen == B::CopyMode && !self.warned_copy {
                    act.warn = Some((B::CopyMode, 0));
                    self.warned_copy = true;
                }
            }
            B::Gone => {
                // Gone 是「確認不存在」的觀測，不是查詢失敗——Prompt 狀態
                // 一併歸零（codex 複核 2026-08-03：否則 Prompt→Gone→Prompt
                // 會被當連續兩次而誤確立；pane id 重用時尤其危險）。
                // 「不歸零」的特權只屬於 Unknown。
                self.prompt_hits = 0;
                self.prompt_since = None;
                self.warned_prompt = false;
                if !self.warned_gone {
                    act.warn = Some((B::Gone, 0));
                    self.warned_gone = true;
                }
            }
            B::Unknown => {}
        }
        act
    }
}

/// cmd_await 的等待主體（唯讀輪詢 status 檔，不寫 events、不取鎖）。
/// CLI 的 `await` 與 evict 的第二段共用同一份（審查 F6：分家就會漂移）。
///
/// 簽名保持不變的薄殼：evict 的呼叫端**顯式不帶 watch**（evict 的逾時語意
/// 不得被 Blocked 提前返回改寫），CLI 要探測時走 `await_task_watched`。
pub fn await_task(paths: &Paths, id: &str, timeout: u64) -> Result<AwaitOutcome> {
    await_task_watched(paths, id, timeout, None)
}

/// `await_task` 的帶探測版本（CLI-AWAIT-3／CLI-AWAIT-4）。
///
/// watch 為 `Some` 時，每 `PROBE_EVERY` 輪對 worker pane 做一次唯讀 blocker
/// 探測（`notify::probe_blocker`，只查詢不送鍵）：
/// - `Prompt` 連續 `PROBE_DEBOUNCE` 次確立後喚 `on_warn` 一次；policy 為
///   `Return` 且持續滿 `grace_secs` → 回 `AwaitOutcome::Blocked`。
/// - `CopyMode`（人在介入）與 `Gone`（pane 沒了）各喚 `on_warn` 一次，
///   **永不**提前返回——前者不是卡死，後者由 task 狀態軸自己收斂。
/// - `Unknown`（查詢層失敗）不警告、**不歸零** Prompt 的去抖與持續計時：
///   查不到不是「已解除」的證據（與 notify 拆出 query-failed 同一理由）。
///
/// **函式內不印任何字**：逾時／警告的呈現由呼叫端決定（on_warn 回呼）。
pub fn await_task_watched(
    paths: &Paths,
    id: &str,
    timeout: u64,
    watch: Option<&BlockerWatch<'_>>,
) -> Result<AwaitOutcome> {
    use crate::notify::PaneBlocker;

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
    let mut poll_round: u32 = 0;
    let mut tracker = BlockerTracker::new();
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
        if let Some(w) = watch {
            poll_round = poll_round.wrapping_add(1);
            if poll_round.is_multiple_of(PROBE_EVERY) {
                let seen: PaneBlocker = crate::notify::probe_blocker(w.tmux, &w.pane);
                let act = tracker.observe(seen, w.policy, w.grace_secs, std::time::Instant::now());
                if let Some((b, sustained)) = act.warn {
                    (w.on_warn)(b, sustained);
                }
                if let Some(since_secs) = act.blocked_since {
                    return Ok(AwaitOutcome::Blocked {
                        status: st,
                        since_secs,
                    });
                }
            }
        }
        std::thread::sleep(poll_sleep(interval));
    }
}

/// 輪詢間隔（秒）→ `Duration`。
///
/// **`try_from_secs_f64` 而非 `from_secs_f64`**：後者對超出 `Duration` 值域的
/// finite 值是 **panic**，而 `await_task` 的驗證只擋掉「非正數／非有限」
/// ——`AGENT_BRIDGE_POLL_INTERVAL=1e300` 這種形狀會一路走到這裡。在 CLI 上
/// 那是 exit 101（codex 複核 major #3）。
/// bash 那邊是把值交給 `sleep`，超大值就睡到天荒地老；`Duration::MAX` 是同一
/// 個終態（處置同 `spawn::wait_ready`）。
fn poll_sleep(interval: f64) -> std::time::Duration {
    std::time::Duration::try_from_secs_f64(interval).unwrap_or(std::time::Duration::MAX)
}

/// 精確翻譯 bash `^([0-9]+|[0-9]*\.[0-9]+)$`：
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

    /// 建 task 目錄失敗但**不是**碰撞時（這裡讓 `tasks_dir` 是一個普通檔案，
    /// `create_dir` 因此拿到 ENOTDIR），訊息 MUST NOT 謊稱「task-id 連續碰撞」，
    /// 且 MUST 帶得出底層 cause。舊寫法 `create_dir(..).is_ok()` 把權限／唯讀 fs／
    /// 路徑非目錄全部誤報成碰撞，還白白重試五次。
    #[test]
    fn non_collision_mkdir_error_is_not_reported_as_collision() {
        let root = std::env::temp_dir().join(format!(
            "ab-core-task-notadir-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // tasks_dir 指向一個普通檔案：其下的 create_dir 必然非碰撞失敗
        let tasks_dir = root.join("tasks-is-a-file");
        std::fs::write(&tasks_dir, b"not a directory").unwrap();

        // 預期 cause：對**同一個形狀**（在普通檔案底下建子目錄）直接取一次
        // `io::Error`，拿它的字串當比對基準。不硬編英文 "Not a directory"——
        // 那串隨 libc／locale 變，硬編會在別的環境變成假紅。
        let expected_cause = std::fs::create_dir(tasks_dir.join("cause-probe"))
            .expect_err("在普通檔案底下建子目錄 MUST 失敗")
            .to_string();

        let paths = Paths {
            data_dir: root.clone(),
            agents_dir: root.join("agents"),
            tasks_dir,
            locks_dir: root.join("locks"),
            state_dir: root.join("state"),
        };
        let err = create_task(
            &paths,
            "sender",
            "receiver",
            &MessageSource::Text("x".into()),
        )
        .expect_err("非碰撞的建目錄失敗 MUST 以錯誤終止");

        let msg = err.to_string();
        assert!(!msg.contains("連續碰撞"), "非碰撞錯誤被誤報成碰撞：{msg}");
        assert!(
            msg.contains("無法建立 task 目錄"),
            "訊息未指出是建 task 目錄失敗：{msg}"
        );
        // 底層 cause MUST 帶得出來：拿掉 production 那句 format 裡的「：{e}」，
        // 這條就要紅。前兩條斷言都不管 cause，這條是它們的補集。
        assert!(
            msg.contains(&expected_cause),
            "訊息未帶出底層 cause（預期含「{expected_cause}」）：{msg}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 極大但 finite 的輪詢間隔 MUST NOT panic（`from_secs_f64` 會）
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

    /// BlockerTracker 去抖／grace／warn-once 的純邏輯驗證（時間注入，不睡眠）。
    #[test]
    fn blocker_tracker_debounces_and_respects_grace() {
        use crate::notify::PaneBlocker as B;
        let t0 = std::time::Instant::now();
        let at = |s: u64| t0 + std::time::Duration::from_secs(s);

        // (1) 單次 Prompt 不觸發任何行為（去抖）
        let mut tr = BlockerTracker::new();
        let a = tr.observe(B::Prompt, BlockerPolicy::Return, 60, at(0));
        assert!(a.warn.is_none() && a.blocked_since.is_none());

        // (2) 第二次確立：warn 一次；grace 未滿不 blocked
        let a = tr.observe(B::Prompt, BlockerPolicy::Return, 60, at(5));
        assert_eq!(a.warn, Some((B::Prompt, 0)));
        assert!(a.blocked_since.is_none());

        // (3) 持續滿 grace → blocked（warn 不重發）
        let a = tr.observe(B::Prompt, BlockerPolicy::Return, 60, at(70));
        assert!(a.warn.is_none());
        assert_eq!(a.blocked_since, Some(65));

        // (4) Warn policy 永不 blocked，即使持續再久
        let mut tw = BlockerTracker::new();
        tw.observe(B::Prompt, BlockerPolicy::Warn, 0, at(0));
        let a = tw.observe(B::Prompt, BlockerPolicy::Warn, 0, at(600));
        assert!(a.blocked_since.is_none());

        // (5) Clear 歸零：去抖重來、解除後允許再警告
        let mut tc = BlockerTracker::new();
        tc.observe(B::Prompt, BlockerPolicy::Return, 60, at(0));
        tc.observe(B::Prompt, BlockerPolicy::Return, 60, at(5));
        tc.observe(B::Clear, BlockerPolicy::Return, 60, at(10));
        let a = tc.observe(B::Prompt, BlockerPolicy::Return, 60, at(15));
        assert!(a.warn.is_none(), "歸零後單次 Prompt 不得觸發");
        let a = tc.observe(B::Prompt, BlockerPolicy::Return, 60, at(20));
        assert_eq!(a.warn, Some((B::Prompt, 0)), "重新確立要再警告一次");

        // (6) Unknown 不歸零也不警告：確立狀態跨越查詢失敗仍持續計時
        let mut tu = BlockerTracker::new();
        tu.observe(B::Prompt, BlockerPolicy::Return, 60, at(0));
        tu.observe(B::Prompt, BlockerPolicy::Return, 60, at(5));
        let a = tu.observe(B::Unknown, BlockerPolicy::Return, 60, at(30));
        assert!(a.warn.is_none() && a.blocked_since.is_none());
        let a = tu.observe(B::Prompt, BlockerPolicy::Return, 60, at(70));
        assert_eq!(a.blocked_since, Some(65), "Unknown 不得重置持續計時");

        // (7) CopyMode／Gone 各警告一次，永不 blocked
        let mut tm = BlockerTracker::new();
        let a = tm.observe(B::CopyMode, BlockerPolicy::Return, 0, at(0));
        assert_eq!(a.warn, Some((B::CopyMode, 0)));
        let a = tm.observe(B::CopyMode, BlockerPolicy::Return, 0, at(5));
        assert!(a.warn.is_none(), "CopyMode 只警告一次");
        let a = tm.observe(B::Gone, BlockerPolicy::Return, 0, at(10));
        assert_eq!(a.warn, Some((B::Gone, 0)));
        let a = tm.observe(B::Gone, BlockerPolicy::Return, 0, at(15));
        assert!(a.warn.is_none() && a.blocked_since.is_none());

        // (8) Gone 歸零 Prompt 狀態（codex 複核 2026-08-03）：
        //     Prompt→Gone→Prompt 不得算連續兩次
        let mut tg = BlockerTracker::new();
        tg.observe(B::Prompt, BlockerPolicy::Return, 0, at(0));
        tg.observe(B::Gone, BlockerPolicy::Return, 0, at(5));
        let a = tg.observe(B::Prompt, BlockerPolicy::Return, 0, at(10));
        assert!(
            a.warn.is_none() && a.blocked_since.is_none(),
            "Gone 之後的單次 Prompt 不得確立"
        );
        //     已確立的 Prompt 經 Gone 後重新出現：grace 起點必須重算
        let mut tg2 = BlockerTracker::new();
        tg2.observe(B::Prompt, BlockerPolicy::Return, 60, at(0));
        tg2.observe(B::Prompt, BlockerPolicy::Return, 60, at(5)); // 確立於 t=5
        tg2.observe(B::Gone, BlockerPolicy::Return, 60, at(10));
        tg2.observe(B::Prompt, BlockerPolicy::Return, 60, at(100));
        let a = tg2.observe(B::Prompt, BlockerPolicy::Return, 60, at(105));
        assert!(
            a.blocked_since.is_none(),
            "Gone 後重新確立，grace 不得沿用舊起點（t=105-105=0 < 60）"
        );
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
        let id = create_task(&paths, "alice", "bob", &MessageSource::Text("hello".into())).unwrap();
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
    /// 目錄（send_rollback 的孤兒目錄問題）。
    #[test]
    fn failed_create_rolls_back_partial_dir() {
        let d = Dir::new("ab-core-task-rb");
        let paths = test_paths(&d);
        let missing = d.path.join("no-such-message-file");
        let err = create_task(&paths, "alice", "bob", &MessageSource::File(missing)).unwrap_err();
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
        let id = create_task(&paths, "a", "b", &MessageSource::Text(b"x".to_vec())).unwrap();
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
        let id = create_task(&paths, "a", "b", &MessageSource::Text("x".into())).unwrap();
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
        let id = create_task(&paths, "a", "b", &MessageSource::Text("x".into())).unwrap();
        let dir = task_dir(&paths, &id);
        assert_eq!(meta_str(&dir, "from").unwrap(), "a");
        assert_eq!(meta_str(&dir, "no_such_field").unwrap(), "null");

        std::fs::write(dir.join("metadata.json"), "{\"from\": null}\n").unwrap();
        assert_eq!(meta_str(&dir, "from").unwrap(), "null");

        std::fs::write(dir.join("metadata.json"), "[1,2]\n").unwrap();
        assert!(meta_str(&dir, "from").is_err(), "非 object 必須整體失敗");
    }

    /// legacy_pinned：舊資料的 metadata 可能帶多餘的尾欄位，`status`／`read`
    /// 路徑照常（泛型 JSON 讀取自然忽略它，不寫遷移）。
    #[test]
    fn legacy_pinned_metadata_is_ignored_by_status_and_read() {
        let d = Dir::new("ab-core-task-legacy");
        let paths = test_paths(&d);
        let id = create_task(&paths, "alice", "bob", &MessageSource::Text("x".into())).unwrap();
        let dir = task_dir(&paths, &id);
        let meta_path = dir.join("metadata.json");
        let meta = std::fs::read_to_string(&meta_path).unwrap();
        let Ok(Value::Object(mut fields)) = json::parse(&meta) else {
            panic!("metadata 應為 object");
        };
        fields.insert("pinned".into(), Value::Bool(true)); // legacy_pinned
        let doc = format!("{}\n", json::render_pretty(&Value::Object(fields)));
        std::fs::write(&meta_path, doc).unwrap();
        let raw = std::fs::read_to_string(&meta_path).unwrap();
        assert!(raw.contains("\"pinned\": true")); // legacy_pinned

        // status 路徑：require_task_dir＋read_status；終態轉換照常改寫 metadata
        update_meta_status(&dir, TaskState::Completed).unwrap();
        let got = require_task_dir(&paths, &id).unwrap();
        assert_eq!(read_status(&got).unwrap(), "completed");

        // read 路徑：with_response 取鎖、驗態、讀標頭、交出 response 路徑
        std::fs::write(dir.join("response.md"), b"done\n").unwrap();
        let (from, to, body) = with_response(&paths, &id, |h, path| {
            Ok((h.from.clone(), h.to.clone(), std::fs::read(path).unwrap()))
        })
        .unwrap();
        assert_eq!((from.as_str(), to.as_str()), ("alice", "bob"));
        assert_eq!(body, b"done\n");
    }

    /// `cancel_task` 是 cancel 語意的單一正本（審查 F6）：轉態＋`cancelled`
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
        let id = create_task(&paths, "alice", "bob", &MessageSource::Text("x".into())).unwrap();
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
        let q = create_task(&paths, "alice", "bob", &MessageSource::Text("x".into())).unwrap();
        let r = create_task(&paths, "alice", "bob", &MessageSource::Text("y".into())).unwrap();
        update_meta_status(&task_dir(&paths, &r), TaskState::Running).unwrap();
        let done = create_task(&paths, "alice", "bob", &MessageSource::Text("z".into())).unwrap();
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

    /// **`created_at` 的磁碟→read-model 管線**（P4.7 切片 C 修正輪 F6）。
    ///
    /// `in_flight` 填這個欄位，而歸屬判定拿它當**唯一**的
    /// 同世代證據。沒有這條斷言的話，loader 哪天誤填成空字串，所有既有測試照樣
    /// 全綠。
    #[test]
    fn created_at_flows_from_metadata_into_the_read_model() {
        let d = Dir::new("ab-core-created-at");
        let paths = test_paths(&d);
        let id = create_task(&paths, "alice", "bob", &MessageSource::Text("x".into())).unwrap();
        // 期望值直接取自磁碟上的 metadata（不是測試自己造一個時間）
        let meta = std::fs::read_to_string(task_dir(&paths, &id).join("metadata.json")).unwrap();
        let Ok(Value::Object(fields)) = json::parse(&meta) else {
            panic!("metadata 應該是 JSON 物件");
        };
        let want = json::jq_raw_field(&fields, "created_at").unwrap_or_default();
        assert!(!want.is_empty(), "create_task 本來就會寫 created_at");
        assert!(
            crate::time::parse_iso_to_epoch(&want).is_some(),
            "寫出來的形狀 MUST 解析得回去（上層的 fail-closed 靠它）"
        );

        let rows = in_flight(&paths);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].created_at, want, "in_flight MUST 原樣帶入");

        // 對照組：metadata 沒有這個欄位（`seed_task` 就不寫）→ 空字串。
        // 上層據此 fail-closed（不可證＝不掛），而不是拿一個假時間去比
        seed_task(&paths, "20260731T000009Z-0009", "queued", None);
        let rows = in_flight(&paths);
        assert!(
            rows.iter()
                .find(|t| t.id == "20260731T000009Z-0009")
                .unwrap()
                .created_at
                .is_empty(),
            "欄位缺失 MUST 是空字串"
        );
    }

    /// payload byte 保真：非 UTF-8 位元組原樣進出，不得 lossy 轉換（分組 6）。
    #[test]
    fn message_file_bytes_are_preserved() {
        let d = Dir::new("ab-core-task-bytes");
        let paths = test_paths(&d);
        let raw: Vec<u8> = vec![0xff, 0xfe, 0x00, 0x41, 0x0a, 0x80];
        let src_file = d.path.join("payload.bin");
        std::fs::write(&src_file, &raw).unwrap();
        let id = create_task(&paths, "a", "b", &MessageSource::File(src_file)).unwrap();
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
}
