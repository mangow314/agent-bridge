//! agent 註冊表 CRUD（state.md STATE-AGENT-*）。對映 bash `cmd_register`:476、
//! `cmd_list`:522、`is_spawned`:153。

use std::path::Path;

use crate::error::{Error, Result};
use crate::fsio::atomic_write;
use crate::json::{self, JsonObject};
use crate::lock::acquire_lock;
use crate::paths::Paths;
use crate::time::now_iso;
use crate::tmux::TmuxClient;
use crate::validate::is_valid_name;
use serde_json::Value;

/// is_spawned:153 的三態對映（STATE-AGENT-2）：`Spawned`／`Manual`／
/// `Undetermined`（非 object、JSON 損壞、讀不到檔案）。**`Undetermined`
/// MUST fail-closed**：呼叫端一律拒絕動作，不得當成 `Manual` 覆寫或除名。
pub enum Provenance {
    Spawned,
    Manual,
    Undetermined,
}

/// 讀 registry 檔判斷出身。對應 bash：
/// `jq -r 'if type != "object" then "bad" elif .spawned == true then "yes" else "no" end'`
pub fn read_provenance(path: &Path) -> Provenance {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Provenance::Undetermined,
    };
    match json::parse(&content) {
        Ok(Value::Object(fields)) => {
            if json::bool_field_is_true(&fields, "spawned") {
                Provenance::Spawned
            } else {
                Provenance::Manual
            }
        }
        _ => Provenance::Undetermined,
    }
}

/// 讀 registry 檔的 `pane_id`。讀不到／損壞回空字串——對映 bash
/// `pane="$(jq -r '.pane_id' "$agent_file" 2>/dev/null)" || pane=""`
/// （cmd_send:619）：空 pane 會被 `notify_pane` 的 PANE_RE 擋下，走既有的
/// notify-failed 降級，不是靜默送鍵到別人的視窗。
pub fn read_pane(path: &Path) -> String {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    match json::parse(&content) {
        Ok(Value::Object(fields)) => json::str_field(&fields, "pane_id")
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// `jq -e '.spawned == true and .ready != true'`（cmd_send:571）：spawn 出身
/// 但尚未回報就緒。用於 send 的「通知可能延後」警告，判不出來一律回 `false`
/// （只是少印一行提示，不影響任何狀態機不變量）。
pub fn is_spawned_not_ready(path: &Path) -> bool {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    match json::parse(&content) {
        Ok(Value::Object(fields)) => {
            json::bool_field_is_true(&fields, "spawned")
                && !json::bool_field_is_true(&fields, "ready")
        }
        _ => false,
    }
}

/// 讀 registry 的任一字串欄位，語意同 `jq -r '.<key> // "<fallback>"'`：
/// 檔案讀不到／不是 object／欄位缺失都回 `fallback`。取值走 `jq -r` 語意
/// （`.runtime` 若被寫成數字，bash 會印出那個數字而不是空字串），
/// fallback 走 `//` 語意（**空字串是 truthy，不觸發 fallback**）。
pub fn read_field(path: &Path, key: &str, fallback: &str) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return fallback.to_string();
    };
    match json::parse(&content) {
        Ok(Value::Object(fields)) => {
            json::jq_alt(&fields, key).unwrap_or_else(|| fallback.to_string())
        }
        _ => fallback.to_string(),
    }
}

/// `jq -e '.<key> == true'` 的比較語意：只認布林 `true`，字串／數字／缺欄位
/// 一律 `false`。讀不到檔案或不是 object 同樣 `false`。
pub fn read_bool(path: &Path, key: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    match json::parse(&content) {
        Ok(Value::Object(fields)) => json::bool_field_is_true(&fields, key),
        _ => false,
    }
}

/// `caller_owner`:884 — 呼叫者定位 `"session_name:@window_id"`。
/// 只認「`TMUX` 與 `TMUX_PANE` 同時存在，且 display-message 真的查得到」：
/// `TMUX_PANE` 可能是繼承自已死 pane 或別台 server 的殘留值。
pub fn caller_owner(tmux: &dyn TmuxClient) -> Option<String> {
    let in_tmux = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
    let pane = std::env::var("TMUX_PANE").ok().filter(|v| !v.is_empty())?;
    if !in_tmux {
        return None;
    }
    let out = tmux
        .exec(&[
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{session_name}:#{window_id}",
        ])?
        .ok_stdout()?;
    // `[[ "$out" == *":@"* ]]`
    if out.contains(":@") { Some(out) } else { None }
}

/// `log_agent_event`:898 — 審計行寫進 `$DATA_DIR/agents.log`，空白分隔固定
/// 6 欄 `<ts> <action> <name> <pane> <runtime> <actor>`。
///
/// **欄位安全在這個唯一咽喉點保證**：name 以外的欄位可能取自 worker 可寫的
/// registry 或可偽造的環境，故全欄摺空白、空值補 `-`，不靠上游自律。
/// `actor` 傳 `None` 時就地解析呼叫者定位（tmux 外為 `-`）。
///
/// 回傳 `Err` 代表 append 失敗——呼叫端多半只揭露不翻盤（不可逆動作已完成）。
pub fn log_agent_event(
    paths: &Paths,
    tmux: &dyn TmuxClient,
    action: &str,
    name: &str,
    pane: &str,
    runtime: &str,
    actor: Option<&str>,
) -> Result<()> {
    let actor = match actor {
        Some(a) => a.to_string(),
        None => caller_owner(tmux).unwrap_or_else(|| "-".to_string()),
    };
    let line = format!(
        "{} {} {} {} {} {}\n",
        now_iso(),
        squash(action),
        squash(name),
        squash(pane),
        squash(runtime),
        squash(&actor),
    );
    let path = paths.data_dir.join("agents.log");
    append_line(&path, &line)
        .map_err(|e| Error::new(format!("無法寫入審計檔 {}：{e}", path.display())))
}

/// `: >> "$DATA_DIR/agents.log"`（cmd_despawn:1517）：以 append 模式實開一次
/// fd 驗證可寫，**不寫入位元組**。不可逆動作之前的預檢——事後才發現審計寫
/// 不了，pane 已死、registry 已刪，呼叫端卻收到失敗而去重試。
pub fn audit_writable(paths: &Paths) -> bool {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.data_dir.join("agents.log"))
        .is_ok()
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())
}

/// bash `${v//[[:space:]]/_}` 後 `[[ -n $v ]] || v="-"`。空白集合取 C locale
/// 的 `[[:space:]]`（space／\t／\n／\v／\f／\r）。
fn squash(v: &str) -> String {
    let s: String = v
        .chars()
        .map(|c| {
            if matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if s.is_empty() { "-".to_string() } else { s }
}

/// cmd_unregister:503 — 移除 agent 註冊（CLI-UNREGISTER-1）。**鎖內檢查出身**：
/// 單純除名 spawned agent 會留下沒人認領的 pane，且讓 cap 少算一個；出身
/// 不明（registry 損壞）同樣拒絕（fail-closed）。
pub fn unregister(paths: &Paths, name: &str) -> Result<()> {
    if !is_valid_name(name) {
        return Err(Error::new(format!(
            "agent 名稱不合法（僅允許 [A-Za-z0-9_-]+）：{name}"
        )));
    }
    let file = paths.agents_dir.join(format!("{name}.json"));
    let guard = acquire_lock(paths, "agents-registry")?;
    let outcome = (|| -> Result<()> {
        if !file.is_file() {
            return Err(Error::new(format!("未註冊的 agent：{name}")));
        }
        match read_provenance(&file) {
            Provenance::Spawned => Err(Error::new(format!(
                "agent '{name}' 是 spawn 出身的 worker，unregister 拒絕（請用 despawn 一併回收 pane）"
            ))),
            Provenance::Undetermined => Err(Error::new(format!(
                "agent '{name}' 的 registry 無法解析，出身不明，unregister 拒絕（若它其實是 spawned，除名會留下沒人回收的 pane）；請確認 {} 後手動處理",
                file.display()
            ))),
            Provenance::Manual => std::fs::remove_file(&file)
                .map_err(|e| Error::new(format!("無法移除 registry 檔 {}：{e}", file.display()))),
        }
    })();
    // 架構 §6：每條 return 路徑都顯式釋放，不倚賴 Drop 作為正確性論證。
    guard.release();
    outcome
}

/// cmd_register:476 — 註冊 agent 與其 tmux pane（CLI-REGISTER-1、
/// STATE-AGENT-1、STATE-AGENT-3）。成功回傳解析出的 pane id。
pub fn register(paths: &Paths, tmux: &dyn TmuxClient, name: &str, target: &str) -> Result<String> {
    if !is_valid_name(name) {
        return Err(Error::new(format!(
            "agent 名稱不合法（僅允許 [A-Za-z0-9_-]+）：{name}"
        )));
    }
    if !tmux.available() {
        return Err(Error::new("找不到 tmux，register 需要 tmux"));
    }
    let pane = tmux
        .resolve_pane(target)
        .ok_or_else(|| Error::new(format!("無法解析 tmux target：{target}")))?;

    let ts = now_iso();
    let doc = JsonObject::new()
        .push_str("name", name)
        .push_str("pane_id", &pane)
        .push_str("registered_at", &ts);
    let file = paths.agents_dir.join(format!("{name}.json"));

    // 取 registry 鎖再寫：與 spawn/despawn/ready 互斥（STATE-AGENT-3）。
    // 架構 §6 紅線：正確性論證只認顯式 release()，Drop 只是「忘了呼叫時」的
    // 便利網——因此下面每一個 return 路徑都手動呼叫 release()，不倚賴函式
    // 結束時的自動 Drop（雖然 Drop 也會正確觸發，但那不是本函式的正確性
    // 論證依據）。
    let guard = acquire_lock(paths, "agents-registry")?;
    if file.exists() {
        match read_provenance(&file) {
            Provenance::Spawned => {
                guard.release();
                return Err(Error::new(format!(
                    "agent '{name}' 是 spawn 出身的 worker，register 拒絕覆寫（要換 pane 請先 despawn）"
                )));
            }
            Provenance::Undetermined => {
                guard.release();
                return Err(Error::new(format!(
                    "agent '{name}' 的 registry 無法解析，出身不明，register 拒絕覆寫；請確認 {} 後手動處理",
                    file.display()
                )));
            }
            Provenance::Manual => {}
        }
    }
    let content = format!("{}\n", doc.render());
    let write_result = atomic_write(&file, content.as_bytes());
    // 比照 bash cmd_register：寫入完成（不論成敗）就釋放鎖，才印訊息／回傳。
    guard.release();
    write_result?;
    Ok(pane)
}

/// cmd_list:522 — CLI-LIST-1。唯讀：`agents/` 目錄本身不存在時視為空集
/// （CLI-RO-1／STATE-GEN-1 唯讀段：目錄缺失≠損壞，不報錯、不建目錄）。
/// 回傳 `(name, pane_id, ready)` 三元組，依檔名排序（對齊 bash glob 的
/// 字典序）。
///
/// **損壞的 registry 檔 MUST NOT 靜默略過**：bash `cmd_list` 在
/// `set -euo pipefail` 下，`jq -r '.name' "$f"` 對非法 JSON／非 object 會
/// 以非零退出，`name="$(...)"` 這個賦值式的命令替換失敗一樣會觸發
/// errexit（已於本機以 `bash -c 'set -e; x="$(jq ... <<<"not json")"'`
/// 實測驗證），使整個 cmd_list 提前中止並帶非零碼——呼叫端只依賴「非零＝
/// 失敗」（spec/cli.md CLI-GEN-1），不依賴確切碼／訊息文字。本函式對齊
/// 這個「一遇到損壞檔就整體失敗」的語意，簡化為 all-or-nothing：全部
/// 檔案都合法解析才回傳完整清單，否則回傳 `Err`（不像 bash 會先印出
/// 已解析成功的前幾行才死──這個部分輸出差異不影響 CLI-GEN-1 的非零
/// 契約，故不強求逐位元重現）。
pub fn list(paths: &Paths) -> Result<Vec<(String, String, String)>> {
    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(&paths.agents_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
            .collect(),
        Err(_) => return Ok(Vec::new()),
    };
    files.sort();

    let mut out = Vec::new();
    for f in files {
        let content = std::fs::read_to_string(&f)
            .map_err(|e| Error::new(format!("無法讀取 registry 檔 {}：{e}", f.display())))?;
        let value = json::parse(&content)
            .map_err(|e| Error::new(format!("registry 檔 {} 解析失敗：{e}", f.display())))?;
        let fields = match value {
            Value::Object(fields) => fields,
            _ => {
                return Err(Error::new(format!(
                    "registry 檔 {} 不是合法的 JSON 物件",
                    f.display()
                )));
            }
        };
        let name = json::str_field(&fields, "name")
            .unwrap_or_default()
            .to_string();
        let pane = json::str_field(&fields, "pane_id")
            .unwrap_or_default()
            .to_string();
        let spawned = json::bool_field_is_true(&fields, "spawned");
        let ready = if spawned {
            if json::bool_field_is_true(&fields, "ready") {
                "ready"
            } else {
                "starting"
            }
        } else {
            "-"
        };
        out.push((name, pane, ready.to_string()));
    }
    Ok(out)
}

/// TUI read model 的一列（tui-design.md §4）。`ready` 沿用 `list` 的三值
/// （`ready`／`starting`／`-`）；`runtime`／`owner` 缺欄位以空字串表示。
/// `spawn_tag`／`registered_at` 也在這裡一起取：TUI 的 `i` 摘要頁需要它們當
/// provenance 證據，而按鍵當下另外開檔重讀會把**不同世代的欄位拼成同一頁**
/// （registry 是 atomic replace，respawn 剛好插在中間就成立）。同一次 parse
/// 取齊才是同一份快照（跨廠審查 major #3）。
pub struct AgentSnapshot {
    pub name: String,
    pub pane: String,
    pub runtime: String,
    pub owner: String,
    pub ready: String,
    pub spawn_tag: String,
    pub registered_at: String,
    /// spawn 出身才有（agents/*.json 既有欄位，read model 補讀，非磁碟 schema
    /// 變更）；人工註冊／缺失＝空字串。P5.3 idle 基準
    /// `max(last_task_at, spawned_at)` 的第二個操作數。
    pub spawned_at: String,
    pub spawned: bool,
    pub corrupt: bool,
    /// lineage 的根（P4.7 切片 A）：**值是 generation key＝canonical
    /// `spawn_tag` 全串**（含 `AGENT_BRIDGE_SPAWN_TAG=` 前綴），不是名稱。
    ///
    /// `Option` 是**契約的一部分**：`None`＝這一列根本沒有這個欄位（legacy
    /// registry，永不 backfill；或人工 `register`），`Some("")`＝欄位在但值
    /// 是空的（寫壞了）。壓成一個空字串會讓「舊資料」與「壞資料」長得一樣，
    /// 而 TUI 的歸組要靠前者存在與否分流。
    pub lineage_root: Option<String>,
    /// 直系 parent 的 canonical `spawn_tag`（同上，值是 generation key）。
    /// 自成根時**欄位缺席**（`None`），不是空字串。
    ///
    /// 僅 provenance／display：**MUST NOT 進入任何 auth／CAS 判斷**
    /// （despawn／evict 的世代綁定照舊只認 `spawn_tag`）。
    pub parent_agent: Option<String>,
}

/// registry 全池唯讀快照（TUI read model，tui-design.md §4）。
///
/// 與 `list` 的 all-or-nothing 語意刻意不同：`list` 是 CLI 契約面（損壞檔
/// MUST 整體非零退出，CLI-GEN-1），dashboard 的職責則是把損壞**顯示出來**
/// 而不是整面消失——同 `spawn::list_long` 對壞檔的處置（整列降級後繼續）。
/// 唯讀：不取鎖、不建目錄；目錄缺失＝空池。
pub fn snapshot(paths: &Paths) -> Vec<AgentSnapshot> {
    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(&paths.agents_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
            .collect(),
        Err(_) => return Vec::new(),
    };
    files.sort();

    let mut out = Vec::new();
    for f in files {
        let base_name = f
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let fields = match std::fs::read_to_string(&f).map(|c| json::parse(&c)) {
            Ok(Ok(Value::Object(m))) => m,
            _ => {
                // 損壞的 registry 照樣佔 cap，必須看得見（同 list_long）
                out.push(AgentSnapshot {
                    name: base_name,
                    pane: String::new(),
                    runtime: String::new(),
                    owner: String::new(),
                    ready: "?".to_string(),
                    spawn_tag: String::new(),
                    registered_at: String::new(),
                    spawned_at: String::new(),
                    spawned: false,
                    corrupt: true,
                    // 損壞檔沒有可信的 lineage 可言——缺席不是「它自成根」，
                    // 是「這一列什麼都證明不了」
                    lineage_root: None,
                    parent_agent: None,
                });
                continue;
            }
        };
        let mut name = json::jq_raw_field(&fields, "name").unwrap_or_default();
        if name.is_empty() {
            name = base_name;
        }
        let spawned = json::bool_field_is_true(&fields, "spawned");
        let ready = if spawned {
            if json::bool_field_is_true(&fields, "ready") {
                "ready"
            } else {
                "starting"
            }
        } else {
            "-"
        };
        out.push(AgentSnapshot {
            name,
            pane: json::jq_raw_field(&fields, "pane_id").unwrap_or_default(),
            runtime: json::jq_raw_field(&fields, "runtime").unwrap_or_default(),
            owner: json::jq_raw_field(&fields, "owner").unwrap_or_default(),
            ready: ready.to_string(),
            spawn_tag: json::jq_raw_field(&fields, "spawn_tag").unwrap_or_default(),
            registered_at: json::jq_raw_field(&fields, "registered_at").unwrap_or_default(),
            spawned_at: json::jq_raw_field(&fields, "spawned_at").unwrap_or_default(),
            spawned,
            corrupt: false,
            // **同一次 parse 取齊**（不另開一次讀檔）：registry 是 atomic
            // replace，分兩次讀會把不同世代的欄位拼進同一列。
            //
            // 走 `string_only_field`：這兩欄的值**必須是字串**（canonical
            // generation key），型別錯誤既不是「沒有這個欄位」也不是一個合法
            // 值。三態＝缺席（`None`）／字串（`Some(原值)`，含空字串）／
            // 非字串（`Some("")`＝invalid 標記）。用 `jq_alt` 會把 `5` 字串
            // 化成 `"5"`、把 `null` 併進缺席——兩種壓平都讓壞資料看起來像
            // 另一種正常狀態（審查 F4）。
            lineage_root: json::string_only_field(&fields, "lineage_root"),
            parent_agent: json::string_only_field(&fields, "parent_agent"),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir {
        path: std::path::PathBuf,
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

    /// **損壞的 registry 檔 MUST NOT 靜默略過**：bash `cmd_list` 在
    /// `set -euo pipefail` 下遇到非法 JSON／非 object 會整體非零退出
    /// （呼叫端只依賴 CLI-GEN-1 的「非零＝失敗」）。換 serde_json 後這條
    /// 仍須成立——一份壞掉的註冊檔不得被當成「沒有這個 agent」。
    #[test]
    fn list_fails_loudly_on_corrupt_registry_file() {
        let d = Dir::new("ab-core-registry-list");
        let paths = test_paths(&d);
        std::fs::write(
            paths.agents_dir.join("good.json"),
            "{\n  \"name\": \"good\",\n  \"pane_id\": \"%1\"\n}\n",
        )
        .unwrap();
        assert_eq!(list(&paths).unwrap().len(), 1);

        // 非法 JSON
        std::fs::write(paths.agents_dir.join("broken.json"), "{ not json").unwrap();
        assert!(list(&paths).is_err(), "損壞檔必須讓 list 整體失敗");

        // 合法 JSON 但不是 object：同樣不是一份 registry
        std::fs::write(paths.agents_dir.join("broken.json"), "null\n").unwrap();
        assert!(list(&paths).is_err(), "非 object 根必須讓 list 整體失敗");
    }

    /// `agents/` 目錄不存在＝空集，不是錯誤（唯讀指令不建目錄，CLI-RO-1）。
    #[test]
    fn list_treats_missing_dir_as_empty() {
        let d = Dir::new("ab-core-registry-empty");
        let paths = Paths {
            data_dir: d.path.clone(),
            agents_dir: d.path.join("agents"),
            tasks_dir: d.path.join("tasks"),
            locks_dir: d.path.join("locks"),
            state_dir: d.path.join("state"),
        };
        assert!(list(&paths).unwrap().is_empty());
        assert!(!paths.agents_dir.exists(), "唯讀路徑不得建目錄");
    }

    /// snapshot 是 dashboard 的 read model：損壞檔降級成單列 `corrupt`，
    /// MUST NOT 讓整份快照消失（與 `list` 的 all-or-nothing 相對照）。
    #[test]
    fn snapshot_degrades_corrupt_file_per_row() {
        let d = Dir::new("ab-core-registry-snap");
        let paths = test_paths(&d);
        std::fs::write(
            paths.agents_dir.join("good.json"),
            "{\n  \"name\": \"good\",\n  \"pane_id\": \"%1\",\n  \"runtime\": \"codex\",\n  \"owner\": \"it:@0\",\n  \"spawned\": true,\n  \"ready\": true\n}\n",
        )
        .unwrap();
        std::fs::write(paths.agents_dir.join("broken.json"), "{ not json").unwrap();

        let rows = snapshot(&paths);
        assert_eq!(rows.len(), 2, "壞檔不得吃掉整份快照");
        // 檔名字典序：broken < good
        assert!(rows[0].corrupt);
        assert_eq!(rows[0].name, "broken");
        assert_eq!(rows[0].ready, "?");
        let g = &rows[1];
        assert!(!g.corrupt);
        assert_eq!(
            (g.name.as_str(), g.pane.as_str(), g.runtime.as_str()),
            ("good", "%1", "codex")
        );
        assert_eq!((g.owner.as_str(), g.ready.as_str()), ("it:@0", "ready"));
        assert!(g.spawned);
    }

    /// 唯讀：`agents/` 缺失＝空池，不建目錄（CLI-RO-1 同向）。
    #[test]
    fn snapshot_missing_dir_is_empty_and_readonly() {
        let d = Dir::new("ab-core-registry-snap-empty");
        let paths = Paths {
            data_dir: d.path.clone(),
            agents_dir: d.path.join("agents"),
            tasks_dir: d.path.join("tasks"),
            locks_dir: d.path.join("locks"),
            state_dir: d.path.join("state"),
        };
        assert!(snapshot(&paths).is_empty());
        assert!(!paths.agents_dir.exists(), "唯讀路徑不得建目錄");
    }

    /// 出身三態：spawned／manual／判不出來（STATE-AGENT-2 的 fail-closed 前提）。
    #[test]
    fn provenance_is_three_valued() {
        let d = Dir::new("ab-core-registry-prov");
        let paths = test_paths(&d);
        let f = paths.agents_dir.join("x.json");

        std::fs::write(&f, "{\"name\":\"x\",\"spawned\":true}").unwrap();
        assert!(matches!(read_provenance(&f), Provenance::Spawned));
        std::fs::write(&f, "{\"name\":\"x\"}").unwrap();
        assert!(matches!(read_provenance(&f), Provenance::Manual));
        std::fs::write(&f, "null").unwrap();
        assert!(matches!(read_provenance(&f), Provenance::Undetermined));
        std::fs::write(&f, "{ not json").unwrap();
        assert!(matches!(read_provenance(&f), Provenance::Undetermined));
        std::fs::remove_file(&f).unwrap();
        assert!(matches!(read_provenance(&f), Provenance::Undetermined));
    }

    /// **lineage 兩欄的三態**（P4.7 切片 A）：缺席（legacy／人工註冊）、
    /// 存在且有值、存在但空——`Option<String>` MUST 把前兩者與第三者分開。
    ///
    /// 壓成 `unwrap_or_default()` 的話，legacy 列與「欄位寫成空字串」在讀者
    /// 眼中一模一樣，而 TUI 的歸組正要靠「有沒有這個欄位」分流。
    #[test]
    fn snapshot_keeps_lineage_fields_three_valued() {
        let d = Dir::new("ab-core-registry-lineage");
        let paths = test_paths(&d);
        let root = "AGENT_BRIDGE_SPAWN_TAG=ab-spawn-root-1-aaaaaaaaaaaa";
        let parent = "AGENT_BRIDGE_SPAWN_TAG=ab-spawn-parent-2-bbbbbbbbbbbb";

        // (a) legacy：spawn 出身但根本沒有這兩個欄位
        std::fs::write(
            paths.agents_dir.join("a-legacy.json"),
            "{\"name\":\"a-legacy\",\"spawned\":true,\"spawn_tag\":\"t-legacy\"}",
        )
        .unwrap();
        // (b) 新式：兩欄都在
        std::fs::write(
            paths.agents_dir.join("b-child.json"),
            format!(
                "{{\"name\":\"b-child\",\"spawned\":true,\"spawn_tag\":\"t-child\",\
                 \"lineage_root\":\"{root}\",\"parent_agent\":\"{parent}\"}}"
            ),
        )
        .unwrap();
        // (c) 自成根：lineage_root 在、parent_agent **缺席**
        std::fs::write(
            paths.agents_dir.join("c-root.json"),
            format!(
                "{{\"name\":\"c-root\",\"spawned\":true,\"spawn_tag\":\"t-root\",\
                 \"lineage_root\":\"{root}\"}}"
            ),
        )
        .unwrap();
        // (d) 寫壞了：欄位在、值是空字串
        std::fs::write(
            paths.agents_dir.join("d-empty.json"),
            "{\"name\":\"d-empty\",\"spawned\":true,\"spawn_tag\":\"t-empty\",\
             \"lineage_root\":\"\",\"parent_agent\":\"\"}",
        )
        .unwrap();
        // (e) 人工註冊：register 不寫這兩欄
        std::fs::write(
            paths.agents_dir.join("e-manual.json"),
            "{\"name\":\"e-manual\",\"pane_id\":\"%9\"}",
        )
        .unwrap();
        // (f) 損壞
        std::fs::write(paths.agents_dir.join("f-broken.json"), "{ not json").unwrap();
        // (g)-(j) **型別錯誤**：欄位在，但值不是字串（審查 F4）
        for (n, v) in [
            ("g-null", "null"),
            ("h-false", "false"),
            ("i-number", "5"),
            ("j-array", "[\"x\"]"),
        ] {
            std::fs::write(
                paths.agents_dir.join(format!("{n}.json")),
                format!(
                    "{{\"name\":\"{n}\",\"spawned\":true,\"spawn_tag\":\"t-{n}\",\
                     \"lineage_root\":{v},\"parent_agent\":{v}}}"
                ),
            )
            .unwrap();
        }

        let snap = snapshot(&paths);
        let get = |n: &str| {
            snap.iter()
                .find(|w| w.name == n)
                .unwrap_or_else(|| panic!("快照缺 {n}"))
        };

        let legacy = get("a-legacy");
        assert_eq!(
            legacy.lineage_root, None,
            "legacy MUST 是缺席（永不 backfill）"
        );
        assert_eq!(legacy.parent_agent, None);

        let child = get("b-child");
        assert_eq!(child.lineage_root.as_deref(), Some(root));
        assert_eq!(child.parent_agent.as_deref(), Some(parent));

        let self_rooted = get("c-root");
        assert_eq!(self_rooted.lineage_root.as_deref(), Some(root));
        assert_eq!(
            self_rooted.parent_agent, None,
            "自成根 MUST 讀成缺席（不是空字串）"
        );

        let empty = get("d-empty");
        assert_eq!(
            empty.lineage_root.as_deref(),
            Some(""),
            "存在但空 MUST 與缺席分得出來"
        );
        assert_eq!(empty.parent_agent.as_deref(), Some(""));
        assert_ne!(
            empty.parent_agent, self_rooted.parent_agent,
            "「寫壞了」與「自成根」MUST NOT 壓成同一態"
        );

        assert_eq!(get("e-manual").lineage_root, None, "register 不寫兩欄");
        assert_eq!(get("e-manual").parent_agent, None);

        let broken = get("f-broken");
        assert!(broken.corrupt);
        assert_eq!(broken.lineage_root, None, "損壞列什麼都證明不了");
        assert_eq!(broken.parent_agent, None);

        // 型別錯誤 MUST 讀成 invalid 標記（`Some("")`）——**不是** `None`
        // （那會謊稱欄位不存在），也**不是**字串化的值（`5` → `"5"` 會讓型別
        // 錯誤看起來像一個合法的 key）
        for n in ["g-null", "h-false", "i-number", "j-array"] {
            let w = get(n);
            assert_eq!(
                w.lineage_root.as_deref(),
                Some(""),
                "{n}：非字串 MUST 是 invalid 標記，不得壓成缺席或字串化"
            );
            assert_eq!(
                w.parent_agent.as_deref(),
                Some(""),
                "{n}：parent_agent 同理"
            );
            assert_ne!(
                w.lineage_root, legacy.lineage_root,
                "{n}：MUST 與缺席分得開"
            );
        }
    }
}
