//! agent 註冊表 CRUD（state.md STATE-AGENT-*）。對映 bash `cmd_register`:476、
//! `cmd_list`:522、`is_spawned`:153。

use std::path::Path;

use crate::error::{Error, Result};
use crate::fsio::atomic_write;
use crate::json::{self, JVal, JsonObject};
use crate::lock::acquire_lock;
use crate::paths::Paths;
use crate::time::now_iso;
use crate::tmux::TmuxClient;
use crate::validate::is_valid_name;

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
        Ok(JVal::Object(fields)) => {
            if json::bool_field_is_true(&fields, "spawned") {
                Provenance::Spawned
            } else {
                Provenance::Manual
            }
        }
        _ => Provenance::Undetermined,
    }
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
            JVal::Object(fields) => fields,
            _ => {
                return Err(Error::new(format!(
                    "registry 檔 {} 不是合法的 JSON 物件",
                    f.display()
                )));
            }
        };
        let name = json::str_field(&fields, "name").unwrap_or_default().to_string();
        let pane = json::str_field(&fields, "pane_id").unwrap_or_default().to_string();
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
