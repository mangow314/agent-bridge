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
}
