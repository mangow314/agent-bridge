//! worker 生命週期：cap、pane 建立、brief 注入、ready 探針、原子回滾、
//! 出身防護（tag）、despawn／evict／idle／disposable（架構 §2 的 `spawn` 列）。
//!
//! 對映 bash：`cmd_spawn`:1063、`spawn_rollback`:940、`rb_kill_tagged`:928、
//! `spawn_wait_ready`:1038、`worker_prompt_arg`:1007、`relay_prompt_arg`:1024、
//! `cmd_despawn`:1476、`cmd_ready`:1594、`cmd_disposable`:1629、`cmd_idle`:1800、
//! `disposable_effective`:444。
//!
//! evict 的三段式編排（send → await → despawn）留在 `ab` CLI 層：它由三個
//! 既有子指令組成，核心邏輯分別已在 `task`／`spawn` 就位。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::{Error, Result};
use crate::fsio::atomic_write;
use crate::json::{self, JsonObject};
use crate::lock::acquire_lock;
use crate::paths::Paths;
use crate::registry::{self, Provenance};
use crate::task;
use crate::time::now_iso;
use crate::tmux::TmuxClient;
use serde_json::Value;

/// `PANE_RE='^%[0-9]+$'`（bin/agent-bridge:30）。
pub fn is_valid_pane(s: &str) -> bool {
    crate::notify::is_valid_pane(s)
}

/// `WINDOW_RE='^@[0-9]+$'`（bin/agent-bridge:33）。
pub fn is_valid_window(s: &str) -> bool {
    s.len() > 1 && s.starts_with('@') && s[1..].bytes().all(|b| b.is_ascii_digit())
}

/// `MODEL_RE='^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'`（bin/agent-bridge:38）。
/// 這個值會被展開進 pane 啟動命令字串，故在解析點就驗。
pub fn is_valid_model(s: &str) -> bool {
    let mut it = s.bytes();
    match it.next() {
        Some(b) if b.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    s.len() <= 64 && it.all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// bash `printf %q` 的逐位元對應。安全字元集以本機 bash 5.3 實測導出
/// （`%+-./0-9:=@A-Z_a-z`），其餘 printable 前置反斜線、控制字元走
/// `$'…'` 形式。
///
/// 為什麼要逐位元對齊而不是「換一種等效引號」：測試 16a4／16a5 直接比對
/// `pane_start_command` 裡的片段（` no_proxy=st\ a\,b exec `），換成單引號
/// 形式雖然語意等價，那兩條斷言會紅。
/// **走位元組而非 `&str`**（codex 複核 2026-07-31）：proxy／PASS_ENV 的值與
/// hooks settings 路徑都可能不是合法 UTF-8，先 `to_string_lossy` 再引號化會把
/// 那些位元組換成 U+FFFD——worker 拿到的就不是呼叫端的值了。`$'\NNN'` 的產物
/// 本身是純 ASCII，所以位元組進、`String` 出，不必把整條啟動指令改成 bytes。
pub fn shell_quote(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "''".to_string();
    }
    let is_ctrl = |c: char| (c as u32) < 0x20 || c == '\u{7f}';
    let text = std::str::from_utf8(bytes).ok();
    // 需要 `$'…'` 形式的兩種情況：含控制字元，或根本不是合法 UTF-8
    let needs_dollar = match text {
        Some(s) => s.chars().any(is_ctrl),
        None => true,
    };
    if needs_dollar {
        let mut out = String::from("$'");
        let push_char = |out: &mut String, c: char| match c {
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{1b}' => out.push_str("\\E"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            c if is_ctrl(c) => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        };
        // 合法 UTF-8 的片段照字元走（bash 在 UTF-8 locale 原樣保留多位元組
        // 字元），解不出來的位元組逐個轉八進位跳脫
        let mut rest = bytes;
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    for c in s.chars() {
                        push_char(&mut out, c);
                    }
                    break;
                }
                Err(e) => {
                    let good = &rest[..e.valid_up_to()];
                    for c in std::str::from_utf8(good).unwrap_or("").chars() {
                        push_char(&mut out, c);
                    }
                    let bad_len = e.error_len().unwrap_or(rest.len() - e.valid_up_to());
                    for b in &rest[e.valid_up_to()..e.valid_up_to() + bad_len] {
                        out.push_str(&format!("\\{b:03o}"));
                    }
                    rest = &rest[e.valid_up_to() + bad_len..];
                }
            }
        }
        out.push('\'');
        return out;
    }
    let safe = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '%' | '+' | '-' | '.' | '/' | ':' | '=' | '@' | '_')
            || !c.is_ascii()
    };
    let mut out = String::new();
    // needs_dollar 為 false ⇒ text 必為 Some
    for c in text.unwrap_or("").chars() {
        if !safe(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `OsStr` 版的 `shell_quote`（Unix 取原始位元組；其他平台退 lossy）。
pub fn shell_quote_os(s: &std::ffi::OsStr) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        shell_quote(s.as_bytes())
    }
    #[cfg(not(unix))]
    {
        shell_quote(s.to_string_lossy().as_bytes())
    }
}

/// relay 專屬參數（cmd_relay:1391 的三處差異）。
pub struct Relay {
    /// 交接檔路徑（已由呼叫端驗過是可讀普通檔、不含單引號）。
    pub handoff: String,
    /// `--self-exit` 指定的前一棒；空字串＝不指定。
    pub prev: String,
    /// 下傳給接手者的鏈深度（本棒 +1）。
    pub depth_next: i64,
}

/// spawn／relay 共用的請求。
pub struct SpawnRequest {
    pub name: String,
    pub runtime: String,
    /// 空字串＝沿用 runtime CLI 的使用者預設模型。
    pub model: String,
    pub use_window: bool,
    pub relay: Option<Relay>,
}

/// `worker_prompt_arg`:1007 — brief 檔＋動態尾巴組成 runtime 的 initial prompt
/// 參數（單一 shell word）。**brief 全文刻意不進命令列**：讓 tmux 的 `sh -c`
/// 在 pane 內展開 `$(cat ...)`，啟動指令因此維持短、`pane_start_command` 仍
/// 可讀。`cat --` 的 option terminator 不可省：brief 路徑是公開覆蓋介面，
/// 一個叫 `--help` 的檔案會被 `cat` 當成選項，spawn 照樣成功卻注入了錯誤的
/// prompt。
fn worker_prompt_arg(name: &str, brief: &str) -> Result<String> {
    // 路徑要以單引號字面值送進 sh；含單引號就湊不出安全的字面值，fail-closed
    if brief.contains('\'') {
        return Err(Error::new(format!(
            "worker brief 路徑不可含單引號：{brief}"
        )));
    }
    Ok(format!(
        "\"$(cat -- '{brief}')  -- The above is your worker brief. Your agent name is {name}. First action: immediately run agent-bridge ready {name}\""
    ))
}

/// `relay_prompt_arg`:1024 — 接手者守則 ＋ 交接檔路徑 ＋（可選）要回收的前一棒。
/// 三道防線同 `worker_prompt_arg`，但這裡更要緊：handoff 路徑來自使用者命令列。
fn relay_prompt_arg(name: &str, brief: &str, handoff: &str, prev: &str) -> Result<String> {
    if brief.contains('\'') {
        return Err(Error::new(format!(
            "接手者 brief 路徑不可含單引號：{brief}"
        )));
    }
    if handoff.contains('\'') {
        return Err(Error::new(format!("交接檔路徑不可含單引號：{handoff}")));
    }
    let tail = if prev.is_empty() {
        String::new()
    } else {
        format!(
            "After taking over, reclaim your predecessor: run agent-bridge despawn {prev} (if it was a manually started session the command is refused — that is normal; keep working)."
        )
    };
    Ok(format!(
        "\"$(cat -- '{brief}')  -- The above is your successor brief. Your agent name is {name}. Handoff file to continue from: {handoff}. First action: immediately run agent-bridge ready {name}, then read that handoff and start on its next steps. {tail}\""
    ))
}

/// spawn 失敗回滾（`spawn_rollback`:940 的 EXIT trap 等價物）。
///
/// **解除回滾靠單一旗標**（`done`）而非逐一清空三個欄位：訊號可能落在兩次
/// 賦值之間，讓回滾看到「有 tag、沒有 registry 路徑」這種半清空狀態，於是
/// 殺了 pane 卻留下 registry。
///
/// 用 `Drop` 是等價實作而非弱化：bash 那邊也是 EXIT trap，同樣不在 SIGKILL
/// 下執行（同 `task::SendRollback` 的論證，架構 §6 對鎖的紅線不適用於此）。
struct SpawnRollback<'a> {
    tmux: &'a dyn TmuxClient,
    done: bool,
    pane: String,
    reg: Option<PathBuf>,
    tag: String,
}

impl SpawnRollback<'_> {
    fn commit(&mut self) {
        self.done = true;
    }

    /// `rb_kill_tagged`:928 — 原子地「驗 tag 才殺」，並確認 pane 真的消失。
    /// 回 `true`＝已不存在。**查詢失敗要回報「無法確認」而不是「已消失」**：
    /// 把 tmux 掛掉當成 pane 不見了，正是 despawn 那條被揪出來的錯誤方向。
    fn kill_tagged(&self, pane: &str) -> bool {
        if !is_valid_pane(pane) {
            return false;
        }
        let pattern = format!("#{{m:\"{} *,#{{pane_start_command}}}}", self.tag);
        let kill = format!("kill-pane -t {pane}");
        // `|| true`：if-shell 判 false 時的非零不該中止回滾
        let _ = self
            .tmux
            .exec(&["if-shell", "-t", pane, "-F", &pattern, &kill]);
        match self.tmux.exec(&["list-panes", "-a", "-F", "#{pane_id}"]) {
            Some(out) if out.status_ok => !out.stdout.lines().any(|l| l == pane),
            _ => false,
        }
    }
}

impl Drop for SpawnRollback<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if !self.tag.is_empty() {
            if !self.pane.is_empty() {
                if !self.kill_tagged(&self.pane.clone()) {
                    eprintln!(
                        "agent-bridge: 警告：回滾未能關閉 pane {p}，請手動確認後 tmux kill-pane -t {p}",
                        p = self.pane
                    );
                }
            } else {
                // pane 已建立但 pane id 沒回到手上：靠啟動指令裡的一次性 tag
                // 掃出孤兒 pane，否則它永遠沒人認領。比對必須錨在啟動指令
                // **開頭**——無邊界的子字串比對會誤殺參數裡碰巧含這串的別人
                // pane。查詢失敗不能吞：迴圈跑零次跟「沒有孤兒」長得一模一樣。
                match self.tmux.exec(&[
                    "list-panes",
                    "-a",
                    "-F",
                    "#{pane_id} #{pane_start_command}",
                ]) {
                    Some(out) if out.status_ok => {
                        let prefix = format!("{} ", self.tag);
                        for line in out.stdout.lines() {
                            let Some((pid, cmd)) = line.split_once(' ') else {
                                continue;
                            };
                            // tmux 會把整條啟動指令加雙引號存（實測 3.7b），
                            // 先剝掉前導引號才錨得住開頭
                            let cmd = cmd.strip_prefix('"').unwrap_or(cmd);
                            if cmd.starts_with(&prefix) && !self.kill_tagged(pid) {
                                eprintln!(
                                    "agent-bridge: 警告：回滾未能關閉孤兒 pane {pid}，請手動確認後 tmux kill-pane -t {pid}"
                                );
                            }
                        }
                    }
                    _ => eprintln!(
                        "agent-bridge: 警告：回滾無法查詢 tmux pane，可能殘留一個啟動指令帶 {} 的孤兒 pane，請手動確認並關閉",
                        self.tag
                    ),
                }
            }
        }
        if let Some(reg) = &self.reg {
            // 刪不掉要講出來：靜默吞掉會留下一筆沒有 pane 的殭屍 registry，
            // 它照樣佔 cap、也擋住同名 spawn
            if std::fs::remove_file(reg).is_err() && reg.exists() {
                eprintln!(
                    "agent-bridge: 警告：回滾未能刪除 registry {}，請手動清除（它仍佔用 spawn 名額）",
                    reg.display()
                );
            }
        }
        self.done = true;
    }
}

/// `cmd_spawn`:1063 — 建立 worker pane 並註冊，回傳 pane id。
///
/// 檢查順序逐條對齊 bash，因為那個順序本身就是契約：**所有會 die 的前提都
/// 必須在建 pane 之前**（brief、hooks settings、readiness 參數、cap），pane
/// 落地後才 die 會留下一個佔著 cap 的孤兒 worker。
pub fn spawn(paths: &Paths, tmux: &dyn TmuxClient, req: &SpawnRequest) -> Result<String> {
    let name = req.name.as_str();
    if !crate::validate::is_valid_name(name) {
        return Err(Error::new(format!(
            "agent 名稱不合法（僅允許 [A-Za-z0-9_-]+）：{name}"
        )));
    }
    if req.runtime.is_empty() {
        return Err(Error::new(
            "spawn 需要 --runtime <runtime>（支援 codex、claude、agy）",
        ));
    }

    // runtime 表：指令 ＋ 守則注入方式。三個 runtime 都走「initial prompt
    // 位置參數」。claude 的旗標選擇理由見 bin/agent-bridge:1093-1110，不複製。
    //
    // agy（Antigravity CLI）的旗標選擇（量測正本 docs/agy-probe.md）：
    // agy 沒有 codex `--profile` 那種細粒度權限路線，也沒有 `--settings` 可讓
    // worker 掛自己的一份，只有粗粒度旗標——姿態由使用者裁決為
    // skip-permissions＋sandbox（2026-07-31），sandbox 實測不擋 agent-bridge
    // CLI 呼叫與專案內寫檔。agy 無 hooks，state 通道不存在，通知恆走 legacy
    // 送鍵（spec/cli.md CLI-SPAWN-1 的 Note）。
    //
    // `--prompt-interactive` 必須是**最後一個旗標**：它不是布林開關而是吃下
    // 一個 token 當值的 string flag（實測 `agy --prompt-interactive
    // --not-a-real-flag --help` rc=0——未知旗標被當成值吞掉，沒有報錯）。
    // 擺在 `--model` 之前的話，`--model` 會被吃成 initial prompt、模型旗標
    // 失效、真正的 prompt 變成錯位的位置參數（跨廠複核 2026-07-31 抓出）。
    // 故 agy 的尾旗標從 runtime_cmd 拆出來，等 --model 附加完再接上去。
    let hooks_settings = config::claude_hooks_settings();
    let (mut runtime_cmd, runtime_tail) = match req.runtime.as_str() {
        "codex" => ("codex --profile agent-worker".to_string(), ""),
        "claude" => (
            format!(
                "claude --permission-mode auto --settings {}",
                shell_quote_os(hooks_settings.as_os_str())
            ),
            "",
        ),
        "agy" => (
            "agy --dangerously-skip-permissions --sandbox".to_string(),
            " --prompt-interactive",
        ),
        other => {
            return Err(Error::new(format!(
                "不支援的 runtime：{other}（支援 codex、claude、agy）"
            )));
        }
    };
    if !req.model.is_empty() {
        runtime_cmd.push_str(&format!(" --model {}", req.model));
    }
    runtime_cmd.push_str(runtime_tail);
    if !tmux.available() {
        return Err(Error::new("找不到 tmux，spawn 需要 tmux"));
    }
    let runtime_bin = runtime_cmd.split(' ').next().unwrap_or("");
    if !crate::tmux::command_exists(runtime_bin) {
        return Err(Error::new(format!("找不到 runtime 指令：{runtime_bin}")));
    }

    // claude hooks settings 存在性預檢：只在 runtime=claude 檢查——codex 分支
    // 不吃 --settings，不該因為這份 claude 專屬檔缺失而 spawn 失敗。要求普通
    // 檔案（-f）而非只驗可讀（-r）：理由同 brief 預檢
    if req.runtime == "claude" && !is_readable_file(&hooks_settings) {
        return Err(Error::new(format!(
            "claude hooks settings 不是可讀的普通檔案：{}（可用 {} 指定）",
            hooks_settings.display(),
            config::ENV_CLAUDE_HOOKS
        )));
    }

    // owner 定位：worker 的落點跟著 orchestrator 走，不跟著使用者的眼睛走。
    // tmux 外呼叫或 TMUX_PANE 已失效則 owner 為空，落點維持舊行為。
    // owner_win 另存 @window_id：new-window 的 -t 是 target-window、拒收 pane id
    let mut owner = registry::caller_owner(tmux).unwrap_or_default();
    let mut owner_win = String::new();
    let mut owner_winname = String::new();
    if !owner.is_empty() {
        owner_win = owner.rsplit(':').next().unwrap_or("").to_string();
        if is_valid_window(&owner_win) {
            if let Ok(p) = std::env::var("TMUX_PANE") {
                owner_winname = tmux
                    .exec(&["display-message", "-p", "-t", &p, "#{window_name}"])
                    .and_then(|o| o.ok_stdout())
                    .unwrap_or_default();
            }
        } else {
            owner.clear();
            owner_win.clear();
        }
    }

    // brief 讀不到就不開 pane：沒有守則的 worker 收到探針只會當成對話回覆。
    // 要驗普通檔案，不能只驗可讀：`[[ -r ]]` 對目錄一樣成立，而 pane 內的
    // `cat` 讀目錄會失敗、命令替換卻仍返回空字串，runtime 照樣被 exec 起來。
    // -f 一併擋掉 FIFO 與裝置檔（cat 會卡住或讀出無關內容）
    let (brief_path, brief_kind, brief_env) = if req.relay.is_some() {
        (
            config::successor_brief(),
            "接手者 brief",
            config::ENV_SUCCESSOR_BRIEF,
        )
    } else {
        (
            config::worker_brief(),
            "worker brief",
            config::ENV_WORKER_BRIEF,
        )
    };
    if !is_readable_file(&brief_path) {
        return Err(Error::new(format!(
            "{brief_kind} 不是可讀的普通檔案：{}（可用 {brief_env} 指定）",
            brief_path.display()
        )));
    }
    // brief 路徑是**原樣嵌進** pane 啟動指令的單引號字面值（`cat -- '<path>'`），
    // 不像 env 值那樣經得起 `$'\NNN'` 跳脫。非 UTF-8 路徑若在這裡走 lossy，
    // pane 內 `cat` 開的會是另一條帶 U+FFFD 的路徑——「驗過可讀、實際讀到別的
    // 東西（或讀不到而注入空守則）」正是 brief 預檢要擋的失效。故 fail-closed
    // 明確拒絕，這是相對 bash 的**刻意偏離**（bash 直接搬位元組），方向是
    // 大聲失敗而非靜默注入錯誤守則（codex 複核 2026-07-31）
    if brief_path.to_str().is_none() {
        return Err(Error::new(format!(
            "{brief_kind} 路徑不是合法 UTF-8，無法安全嵌入 pane 啟動指令：{}（可用 {brief_env} 指定）",
            brief_path.display()
        )));
    }

    let (ready_timeout, _) = config::ready_opts()?;
    let max = config::max_spawn()?;

    // cap 檢查、建 pane、註冊全部包在 registry 鎖內，杜絕並行 spawn 繞過 cap；
    // 鎖內任一步失敗 → 回滾（kill 已建 pane＋刪 registry 檔）
    let guard = acquire_lock(paths, "agents-registry")?;
    let mut rb = SpawnRollback {
        tmux,
        done: false,
        pane: String::new(),
        reg: None,
        tag: String::new(),
    };
    let outcome = spawn_locked(
        paths,
        tmux,
        req,
        &mut rb,
        SpawnLocked {
            runtime_cmd: &runtime_cmd,
            brief_path: &brief_path,
            owner: &owner,
            owner_win: &owner_win,
            owner_winname: &owner_winname,
            max,
        },
    );
    // 回滾必須在放鎖之前跑完（bash：`trap 'spawn_rollback || true; release_lock'`）
    let pane = match outcome {
        Ok(pane) => {
            rb.commit();
            pane
        }
        Err(e) => {
            drop(rb);
            guard.release();
            return Err(e);
        }
    };
    drop(rb);
    guard.release();

    // 這之後 worker 已經存在且註冊在案，任何失敗都不該讓呼叫端以為 spawn 失敗
    // ——它會照著「失敗」去重試或放棄，實際卻留著一個佔 cap 的 worker
    let _ = writeln_stderr(&format!(
        "已 spawn agent '{name}' → pane {pane}（runtime：{}）",
        req.runtime
    ));
    let _ = writeln_stdout(&pane);
    spawn_wait_ready(paths, tmux, name, &pane, ready_timeout);
    Ok(pane)
}

/// 傳給鎖內主體的唯讀參數束（避免十個位置參數）。
struct SpawnLocked<'a> {
    runtime_cmd: &'a str,
    brief_path: &'a Path,
    owner: &'a str,
    owner_win: &'a str,
    owner_winname: &'a str,
    max: i64,
}

fn spawn_locked(
    paths: &Paths,
    tmux: &dyn TmuxClient,
    req: &SpawnRequest,
    rb: &mut SpawnRollback,
    ctx: SpawnLocked,
) -> Result<String> {
    let name = req.name.as_str();
    let reg_file = paths.agents_dir.join(format!("{name}.json"));
    if reg_file.exists() {
        return Err(Error::new(format!(
            "agent '{name}' 已註冊，spawn 不覆蓋（人工註冊請先 unregister；spawned 請先 despawn）"
        )));
    }

    // 無法解析的 registry 保守計入 cap：漏算會讓上限形同虛設，寧可少開一個
    let mut count: i64 = 0;
    for f in registry_files(paths) {
        if !matches!(registry::read_provenance(&f), Provenance::Manual) {
            count += 1;
        }
    }
    if count >= ctx.max {
        return Err(Error::new(format!(
            "已達 spawn 上限（{}={}，現有 spawned agent {count} 個）；先 despawn 閒置 worker",
            config::ENV_MAX_SPAWN,
            ctx.max
        )));
    }

    // 一次性 tag 先於建 pane 設好：pane 一旦誕生就認得出是誰的，即使 pane id
    // 沒能回到手上，回滾仍掃得到。tag 是刪除的依據，熵不能省（48 位）；agent
    // 名字也編進去——registry 位在 worker 可寫的資料目錄，若 tag 與名字無關，
    // 被注入的 worker A 只要把 B 的 tag 抄進自己的 registry，就能讓
    // orchestrator 的 `despawn A` 殺掉 B 的 pane
    let tag = format!("ab-spawn-{name}-{}-{}", std::process::id(), secure_hex12()?);
    let prompt_arg = match &req.relay {
        Some(r) => relay_prompt_arg(name, &ctx.brief_path.to_string_lossy(), &r.handoff, &r.prev)?,
        None => worker_prompt_arg(name, &ctx.brief_path.to_string_lossy())?,
    };

    // proxy 環境穿透：pane 的環境繼承自 tmux server，不是 spawn 呼叫者——
    // orchestrator shell 才有的 proxy 變數到不了 worker，在強制走 proxy 的
    // 網路裡 runtime 會直連而死。值以 `printf %q` 逐個跳脫（同信任域，跳脫
    // 是防意外拆詞，不是防注入）；空值照傳。tag 必須維持第一個 token
    let mut env_prefix = String::new();
    for pv in [
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ] {
        if let Some(v) = std::env::var_os(pv) {
            env_prefix.push_str(&format!("{pv}={} ", shell_quote_os(&v)));
        }
    }
    // 白名單延伸：只帶「已設」的變數（未設就跳過，不塞空值）
    for pe in config::pass_env_names()? {
        if let Some(v) = std::env::var_os(&pe) {
            env_prefix.push_str(&format!("{pe}={} ", shell_quote_os(&v)));
        }
    }
    // 接力鏈深度只在 relay 路徑下傳：spawn 出來的 worker 不是接力鏈的一環
    if let Some(r) = &req.relay {
        env_prefix.push_str(&format!("{}={} ", config::ENV_RELAY_DEPTH, r.depth_next));
    }

    let spawn_tag_env = format!("{}={tag}", config::ENV_SPAWN_TAG);
    let tagged_cmd = format!(
        "{spawn_tag_env} {env_prefix}exec {} {prompt_arg}",
        ctx.runtime_cmd
    );
    // 回滾比對的是「env 前綴＋一個空白」
    rb.tag = spawn_tag_env.clone();

    // per-owner worker window：同 owner 先前的 worker window 還活著就沿用
    // （split 進去＋tiled 重排），否則新建一個緊鄰 orchestrator window 之後。
    // 查表在 registry 鎖內：兩個並行 spawn 不會為同一 owner 各開一窗
    let worker_win = if ctx.owner.is_empty() || req.use_window {
        String::new()
    } else {
        find_worker_window(paths, tmux, ctx.owner)
    };

    let pane_out = if req.use_window {
        if !ctx.owner.is_empty() {
            // 錨定在 orchestrator window 之後；不帶 -t 的落點跟著 client 焦點走
            new_window(tmux, &["-dP", "-a", "-t", ctx.owner_win], &tagged_cmd)
        } else {
            new_window(tmux, &["-dP"], &tagged_cmd)
        }
    } else if !worker_win.is_empty() {
        // 先取 pane 再重排：bash 的 `select-layout` 在 `|| die` 之後才執行
        // （:1299-1301），split 失敗時根本不會走到
        let p = tmux
            .exec(&[
                "split-window",
                "-dP",
                "-t",
                &worker_win,
                "-F",
                "#{pane_id}",
                &tagged_cmd,
            ])
            .and_then(|o| o.ok_stdout())
            .ok_or_else(|| Error::new("tmux split-window 失敗"))?;
        let _ = tmux.exec(&["select-layout", "-t", &worker_win, "tiled"]);
        Ok(p)
    } else if !ctx.owner.is_empty() {
        new_window(
            tmux,
            &[
                "-dP",
                "-a",
                "-t",
                ctx.owner_win,
                "-n",
                &worker_window_name(ctx.owner_winname),
            ],
            &tagged_cmd,
        )
    } else {
        // tmux 外呼叫（腳本、CI、測試）：無從得知 owner，維持舊行為切在目前視窗
        tmux.exec(&["split-window", "-dP", "-F", "#{pane_id}", &tagged_cmd])
            .and_then(|o| o.ok_stdout())
            .ok_or_else(|| Error::new("tmux split-window 失敗"))
    };
    let pane = pane_out?;
    if pane.is_empty() {
        return Err(Error::new("tmux 未回傳 pane id"));
    }
    rb.pane = pane.clone();

    // 立刻夭折偵測：runtime 指令不存在／啟動即出錯時 pane 會瞬間消失
    std::thread::sleep(std::time::Duration::from_millis(300));
    if !tmux.pane_exists(&pane) {
        return Err(Error::new(format!(
            "runtime 啟動即失敗（pane {pane} 已消失）：{}",
            ctx.runtime_cmd
        )));
    }

    // 落點記錄與身分標示：worker_window 供同 owner 後續 spawn 沿用（--window 的
    // 專屬視窗不共用故不寫）；pane 標題讓多 worker 同窗時邊框直接可辨識
    let mut placed_win = tmux
        .exec(&["display-message", "-p", "-t", &pane, "#{window_id}"])
        .and_then(|o| o.ok_stdout())
        .unwrap_or_default();
    if !is_valid_window(&placed_win) {
        placed_win.clear();
    }
    let reg_win = if !ctx.owner.is_empty() && !req.use_window {
        placed_win
    } else {
        String::new()
    };
    if !reg_win.is_empty() {
        let set = tmux.exec(&["set-option", "-w", "-t", &reg_win, "@ab_owner", ctx.owner]);
        let ok = matches!(&set, Some(o) if o.status_ok);
        if worker_win.is_empty() && !ok {
            // 新建：這筆寫入是 trust root 的建立，失敗必須翻盤——靜默吞掉會做出
            // 「spawn 成功但永不可沿用」的窗。此點在 rb.pane 之後、commit 之前，
            // 回滾會帶走 pane（sole-pane window 隨 pane 一同消失）
            return Err(Error::new(
                "worker window 所有權印記（@ab_owner）寫入失敗，回滾 spawn",
            ));
        }
        // 沿用：印記已驗證存在且正確才走得進來，重寫只是冪等，失敗無害
    }
    let title = if req.model.is_empty() {
        format!("{name} ({})", req.runtime)
    } else {
        format!("{name} ({}/{})", req.runtime, req.model)
    };
    let _ = tmux.exec(&["select-pane", "-t", &pane, "-T", &title]);
    if !ctx.owner.is_empty() {
        let _ = tmux.exec(&["set-option", "-w", "-t", &pane, "pane-border-status", "top"]);
    }

    // 行程身分（STATE-AGENT-4）：hook 端用它取代時間窗判所有權（HOOK-OWNER-5）。
    // 兩欄要嘛都有、要嘛都空——只有一半的話 hook 端無法判斷 pid 是否已被重用，
    // 而「不確定」在這條路上一律要落回時間窗
    let (worker_pid, worker_starttime) = resolve_worker_identity(tmux, &pane, &req.runtime);
    let ts = now_iso();
    let doc = JsonObject::new()
        .push_str("name", name)
        .push_str("pane_id", &pane)
        .push_str("registered_at", &ts)
        .push_bool("spawned", true)
        .push_str("runtime", &req.runtime)
        .push_str("model", &req.model)
        .push_str("spawned_at", &ts)
        .push_bool("ready", false)
        .push_str("spawn_tag", &spawn_tag_env)
        .push_str("owner", ctx.owner)
        .push_str("worker_window", &reg_win)
        .push_str("worker_pid", &worker_pid)
        .push_str("worker_starttime", &worker_starttime);
    rb.reg = Some(reg_file.clone());
    atomic_write(&reg_file, format!("{}\n", doc.render()).as_bytes())?;
    let actor = if ctx.owner.is_empty() { "-" } else { ctx.owner };
    // **審計失敗必須翻盤**（分組 19c/19c'/19d/19e）：bash 在 `set -e` 下讓
    // `log_agent_event` 的非零直接帶走 cmd_spawn，EXIT trap 於是回滾 pane 與
    // registry。吞掉的話會留下一個沒有審計線、卻佔著 cap 的 worker，而呼叫端
    // 看到的是成功。這是 spawn 少數「審計比不可逆動作先發生」的位置——pane 還
    // 在回滾範圍內，所以這裡可以硬起來
    registry::log_agent_event(
        paths,
        tmux,
        "spawned",
        name,
        &pane,
        &req.runtime,
        Some(actor),
    )?;
    Ok(pane)
}

/// worker window 的名字：owner 所在 window 名冠上 `ab:` 前綴。
///
/// **冠之前必須把既有的 `ab:` 前綴全部剝掉**：orchestrator 自己就可能坐在
/// `ab:` 開頭的 window（relay 接棒、或從 worker window 內再 spawn），直接冠
/// 會逐代累積成 `ab:ab:ab:…`（使用者實測回報，現場堆到七層）。剝到空字串
/// （owner window 就叫 `ab:`）退回 `workers`，與 owner_winname 為空時同解。
pub fn worker_window_name(owner_winname: &str) -> String {
    let mut base = owner_winname;
    while let Some(rest) = base.strip_prefix("ab:") {
        base = rest;
    }
    if base.is_empty() {
        "ab:workers".to_string()
    } else {
        format!("ab:{base}")
    }
}

fn new_window(tmux: &dyn TmuxClient, opts: &[&str], cmd: &str) -> Result<String> {
    let mut args: Vec<&str> = vec!["new-window"];
    args.extend_from_slice(opts);
    args.extend_from_slice(&["-F", "#{pane_id}", cmd]);
    tmux.exec(&args)
        .and_then(|o| o.ok_stdout())
        .ok_or_else(|| Error::new("tmux new-window 失敗"))
}

/// 回查同 owner 可沿用的 worker window（cmd_spawn:1262-1288）。
///
/// **反 confused-deputy**：registry 對每個 worker 可寫，`@id` 語法合法、window
/// 存在都冒充得了。信任根源放在攻擊面之外——建窗時把 owner 印記寫進 tmux 視窗
/// 選項 `@ab_owner`，沿用前驗證其值等於「本次 live 解析」的 owner；只能寫
/// registry 的攻擊者碰不到 tmux 選項。
/// STATE-AGENT-4：取 worker runtime 行程的 `(pid, starttime)`，取不到就兩欄
/// 皆空（＝hook 端落回時間窗判別）。
///
/// **這裡的保守是有方向的**：記錯 pid 比不記更糟——本尊 hook 的 PPID 會與錯
/// 的 pid 不符，M5 的自癒整條失效。所以身分一律走 `proc::attest_runtime`：
/// `pane_pid` 的 cmdline 形狀確實是本次 runtime、且前後兩次 starttime 相同，
/// 才採用；任何一步取不到、對不上，都退回空字串。
fn resolve_worker_identity(tmux: &dyn TmuxClient, pane: &str, runtime: &str) -> (String, String) {
    let empty = (String::new(), String::new());
    if !crate::proc::available() {
        return empty;
    }
    let Some(pid) = tmux
        .exec(&["display-message", "-p", "-t", pane, "#{pane_pid}"])
        .and_then(|o| o.ok_stdout())
    else {
        return empty;
    };
    let pid = pid.trim().to_string();
    // 夾了 shell、runtime 是 wrapper script、或 pid 已經不在了 → 空欄 fallback
    match crate::proc::attest_runtime(&pid, runtime) {
        Some(st) => (pid, st),
        None => empty,
    }
}

fn find_worker_window(paths: &Paths, tmux: &dyn TmuxClient, owner: &str) -> String {
    for wf in registry_files(paths) {
        let Ok(content) = std::fs::read_to_string(&wf) else {
            continue;
        };
        let Ok(Value::Object(fields)) = json::parse(&content) else {
            continue;
        };
        if json::jq_raw_field(&fields, "owner").unwrap_or_default() != owner {
            continue;
        }
        let Some(ww) = json::jq_raw_field(&fields, "worker_window") else {
            continue;
        };
        if !is_valid_window(&ww) {
            continue;
        }
        let alive = matches!(
            tmux.exec(&["list-windows", "-a", "-F", "#{window_id}"]),
            Some(ref o) if o.status_ok && o.stdout.lines().any(|l| l == ww)
        );
        if !alive {
            continue;
        }
        // 未設選項時 show-options 以非零收場（實測 tmux 3.7b）
        let Some(ww_owner) = tmux
            .exec(&["show-options", "-wv", "-t", &ww, "@ab_owner"])
            .and_then(|o| o.ok_stdout())
        else {
            continue;
        };
        if ww_owner == owner {
            return ww;
        }
    }
    String::new()
}

/// `spawn_wait_ready`:1038 — 間隔重送探針（`agent-bridge ready <name>`）直到
/// registry 翻 ready 或逾時。REPL 啟動期吃掉的按鍵靠重送覆蓋；**逾時不回滾、
/// 僅警告**，pane 留用供人工診斷。
fn spawn_wait_ready(paths: &Paths, tmux: &dyn TmuxClient, name: &str, pane: &str, timeout: u64) {
    if timeout == 0 {
        return;
    }
    // 兩個參數已由 config::ready_opts 在建 pane 前驗過；此處重讀取間隔值
    let interval = config::ready_opts().map(|(_, i)| i).unwrap_or(2.0);
    let reg = paths.agents_dir.join(format!("{name}.json"));
    let started = std::time::Instant::now();
    loop {
        // `jq -e '.ready == true'` 的比較語意：只認布林 true。用取值語意
        // （`jq -r`）的話 `"ready": "true"` 這種字串也會過，那是 worker 可寫
        // 的欄位，不該讓它比 bash 更寬鬆
        if registry::read_bool(&reg, "ready") {
            let _ = writeln_stderr(&format!("agent '{name}' 已回報就緒（ready）"));
            return;
        }
        if started.elapsed().as_secs() >= timeout {
            eprintln!(
                "agent-bridge: 警告：agent '{name}' 於 {timeout}s 內未回報就緒；pane {pane} 留用供診斷，可於該 pane 手動執行：agent-bridge ready {name}"
            );
            return;
        }
        let _ = crate::notify::notify_pane(tmux, pane, &format!("agent-bridge ready {name}"));
        // `from_secs_f64` 對非有限／溢位的值是 **panic**，而這裡已經在
        // 「registry 寫完、回滾解除」之後——panic 會讓一個已經活著的 worker 被
        // 呼叫端當成 spawn 失敗（codex 複核 2026-07-31）。bash 那邊是把值交給
        // `sleep`，超大值就睡到天荒地老；`Duration::MAX` 是同一個終態。
        let d =
            std::time::Duration::try_from_secs_f64(interval).unwrap_or(std::time::Duration::MAX);
        std::thread::sleep(d);
    }
}

/// despawn 的三種終局（bash `DESPAWN_RESULT`）。evict 靠 `Stale` 判斷「registry
/// 清掉了，但那個 pane 還活著、已經不屬於這個 agent」——那不是一次回收。
#[derive(PartialEq, Eq, Debug)]
pub enum DespawnResult {
    Killed,
    Absent,
    Stale,
}

/// evict 傳給 despawn 的旁路參數（bash 的兩個全域變數）。
#[derive(Default)]
pub struct DespawnCtx {
    /// generation 綁定：registry 的 `spawn_tag` 必須等於這個值才准回收。
    pub expect_tag: Option<String>,
    /// evict 已走過收尾流程，審計不必記 `despawned-unsaved`。
    pub notes_handled: bool,
}

/// `cmd_despawn`:1476 — 回收 spawn 出身的 worker（kill pane＋除名）。
///
/// 出身檢查、generation 比對、kill 全在 registry 鎖內：檢查與 kill-pane 之間
/// 若 registry 被同名 register 換掉，鎖外檢查會拿舊紀錄放行、卻殺到新的人工
/// pane（TOCTOU）。
pub fn despawn(
    paths: &Paths,
    tmux: &dyn TmuxClient,
    name: &str,
    ctx: &DespawnCtx,
) -> Result<DespawnResult> {
    if !crate::validate::is_valid_name(name) {
        return Err(Error::new(format!(
            "agent 名稱不合法（僅允許 [A-Za-z0-9_-]+）：{name}"
        )));
    }
    let f = paths.agents_dir.join(format!("{name}.json"));
    let guard = acquire_lock(paths, "agents-registry")?;
    let outcome = despawn_locked(paths, tmux, name, &f, ctx);
    guard.release();
    outcome
}

fn despawn_locked(
    paths: &Paths,
    tmux: &dyn TmuxClient,
    name: &str,
    f: &Path,
    ctx: &DespawnCtx,
) -> Result<DespawnResult> {
    if !f.is_file() {
        return Err(Error::new(format!("未註冊的 agent：{name}")));
    }
    match registry::read_provenance(f) {
        Provenance::Manual => {
            return Err(Error::new(format!(
                "agent '{name}' 非 spawn 出身，despawn 拒絕（人工註冊請用 unregister）"
            )));
        }
        Provenance::Undetermined => {
            return Err(Error::new(format!(
                "agent '{name}' 的 registry 無法解析，出身不明，despawn 拒絕；請確認 {} 後手動處理",
                f.display()
            )));
        }
        Provenance::Spawned => {}
    }

    // pane_id 不帶 `//` 預設（bash `jq -r '.pane_id'`）：缺欄位印字面 `null`，
    // 於是走下面那條「格式不合法」的 die，訊息裡也照樣是 `null`
    let pane = registry::read_field(f, "pane_id", "null");
    let runtime = registry::read_field(f, "runtime", "-");
    let tag = registry::read_field(f, "spawn_tag", "");

    // generation 綁定（evict 專用）：await 與 despawn 之間同名 agent 可能已經
    // 換了一代。只憑名字就殺，會殺掉一個沒收過收尾任務、也沒宣告 disposable
    // 的新 worker——那正是這一層唯一不可接受的失效
    if let Some(expect) = &ctx.expect_tag
        && !expect.is_empty()
        && &tag != expect
    {
        return Err(Error::new(format!(
            "agent '{name}' 已不是 evict 當初鎖定的那一個（spawn tag 不符，期間被換過一代）；拒絕回收以免殺掉未收尾的 worker"
        )));
    }
    // pane_id 會被展開進 tmux 命令字串（if-shell 的 kill-pane），而 `;` 在
    // tmux 命令中是分隔符——不驗格式的話 registry 就是一條命令注入通道
    if !is_valid_pane(&pane) {
        return Err(Error::new(format!(
            "registry 的 pane_id 格式不合法（疑似遭竄改），拒絕操作：{pane}"
        )));
    }
    if !tmux.available() {
        return Err(Error::new("找不到 tmux，despawn 無法確認 pane 是否回收"));
    }

    // 審計先驗可寫再動手：kill pane 與刪 registry 都不可逆，事後 append 失敗
    // 會讓呼叫端以為 despawn 失敗而去重試
    if !registry::audit_writable(paths) {
        return Err(Error::new(format!(
            "agents.log 不可寫（審計無法落地），despawn 拒絕動手：{}",
            paths.data_dir.join("agents.log").display()
        )));
    }

    // 查詢失敗（tmux server 不可達、sandbox 擋 socket）與「pane 不存在」是兩件
    // 事：前者無從判斷，不能當成 pane 已死而逕自清 registry
    let listing = tmux
        .exec(&["list-panes", "-a", "-F", "#{pane_id} #{pane_start_command}"])
        .ok_or_else(|| Error::new("無法查詢 tmux pane（registry 保留不動，請排除後重試）："))?;
    if !listing.status_ok {
        return Err(Error::new(format!(
            "無法查詢 tmux pane（registry 保留不動，請排除後重試）：{}",
            merged(&listing)
        )));
    }
    let mut found = false;
    let mut live_cmd = String::new();
    for line in listing.stdout.lines() {
        let (pid, cmd) = line.split_once(' ').unwrap_or((line, ""));
        if pid == pane {
            found = true;
            live_cmd = cmd.strip_prefix('"').unwrap_or(cmd).to_string();
            break;
        }
    }

    if found {
        // pane id 對得上還不夠：id 會被重用，registry 也可能被寫入資料目錄的
        // worker 竄改。啟動指令帶不帶我們當初埋的 tag，才是出身證據。tag 本身
        // 也要驗格式：否則 registry 裡填個 spawn_tag="bash" 就成了萬用鑰匙
        if !tag_shape_ok(&tag, name) || !live_cmd.starts_with(&format!("{tag} ")) {
            std::fs::remove_file(f).map_err(|_| {
                Error::new(format!(
                    "pane {pane} 已非本 agent 所有，且 registry 刪除失敗，請手動清除 {}",
                    f.display()
                ))
            })?;
            // 預檢之後 append 仍可能失敗（ENOSPC 等）：不可逆動作已完成，
            // 這時只能誠實揭露，不能以非零收場誤導呼叫端重試
            if registry::log_agent_event(paths, tmux, "despawn-stale", name, &pane, &runtime, None)
                .is_err()
            {
                eprintln!(
                    "agent-bridge: 警告：registry 已清除，但審計未落地（agents.log append 失敗）"
                );
            }
            eprintln!(
                "agent-bridge: 警告：pane {pane} 目前的啟動指令不帶 '{name}' 的 spawn tag（id 已被別的 pane 佔用，或 registry 遭竄改）；已清除註冊，未動該 pane"
            );
            return Ok(DespawnResult::Stale);
        }
        // 驗證與 kill 分成兩次 tmux 呼叫，中間 server 若死掉重啟、新 server 把
        // 同一個 %N 發給人工 pane，第二次呼叫就會殺錯人。if-shell 讓「再驗一次
        // tag」與 kill 在同一次 client 連線內完成
        let pattern = format!("#{{m:\"{tag} *,#{{pane_start_command}}}}");
        let kill = format!("kill-pane -t {pane}");
        let _ = tmux.exec(&["if-shell", "-t", &pane, "-F", &pattern, &kill]);
        let after = tmux
            .exec(&["list-panes", "-a", "-F", "#{pane_id}"])
            .ok_or_else(|| {
                Error::new(format!(
                    "kill 後無法確認 pane {pane} 狀態（registry 保留不動，請排除後重試）："
                ))
            })?;
        if !after.status_ok {
            return Err(Error::new(format!(
                "kill 後無法確認 pane {pane} 狀態（registry 保留不動，請排除後重試）：{}",
                merged(&after)
            )));
        }
        if after.stdout.lines().any(|l| l == pane) {
            return Err(Error::new(format!(
                "無法關閉 pane {pane}（kill 失敗，或 tmux server 在驗證後被替換）；registry 保留不動，請排除後重試"
            )));
        }
    }

    // 審計要看得出「這次回收有沒有丟掉東西」。despawn 是公開指令，任何人都能
    // 繞過 evict 直接收掉一個沒宣告 disposable 的 worker——機制上不擋（擋了會
    // 逼出 --force），但不能讓審計線看起來跟一次乾淨的回收一模一樣
    let ev = if !ctx.notes_handled && !disposable_effective(paths, name, f) {
        "despawned-unsaved"
    } else {
        "despawned"
    };
    // **刪不掉不得謊報成功**（codex 複核 2026-07-31 blocker）：bash 這行是裸的
    // `rm -f -- "$f"`（:1580），失敗在 `set -e` 下直接把 despawn 帶成非零，
    // 不會寫審計、也不會印「已 despawn」。吞掉的話會留下一筆仍佔 cap、也擋住
    // 同名 spawn 的殭屍 registry，而呼叫端（含 evict）以為收乾淨了。
    // `rm -f` 對「本來就不存在」是成功，故 NotFound 照樣往下走。
    match std::fs::remove_file(f) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Error::new(format!(
                "pane 已回收，但 registry {} 刪除失敗，請手動清除（它仍佔用 spawn 名額）：{e}",
                f.display()
            )));
        }
    }
    if registry::log_agent_event(paths, tmux, ev, name, &pane, &runtime, None).is_err() {
        eprintln!(
            "agent-bridge: 警告：pane 已回收、registry 已刪，但審計未落地（agents.log append 失敗）"
        );
    }
    if found {
        let _ = writeln_stderr(&format!("已 despawn agent '{name}'（pane {pane}）"));
        Ok(DespawnResult::Killed)
    } else {
        let _ = writeln_stderr(&format!(
            "已 despawn agent '{name}'（pane {pane} 已不存在，僅清除註冊）"
        ));
        Ok(DespawnResult::Absent)
    }
}

/// spawn tag 的 48 位隨機尾綴。**熵讀不到就失敗，不退回可預測值**
/// （codex 複核 2026-07-31）：這個值是 despawn 的殺人依據，不是
/// `task::rand_suffix` 那種「只為避開同秒碰撞、還有重試迴圈兜底」的用途。
/// bash 那邊 `head -c 2 /dev/urandom` 失敗會讓命令替換在 `set -e` 下把
/// cmd_spawn 帶走——同樣是建 pane 之前就死。
fn secure_hex12() -> Result<String> {
    use std::io::Read;
    let mut buf = [0u8; 6];
    let mut f = std::fs::File::open("/dev/urandom")
        .map_err(|e| Error::new(format!("無法取得 spawn tag 的隨機值（/dev/urandom）：{e}")))?;
    f.read_exact(&mut buf)
        .map_err(|e| Error::new(format!("無法取得 spawn tag 的隨機值（/dev/urandom）：{e}")))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// bash `^AGENT_BRIDGE_SPAWN_TAG=ab-spawn-<name>-[0-9]+-[0-9a-f]{12}$`。
/// 格式含本 agent 的名字，抄別人的 tag 也對不上。
fn tag_shape_ok(tag: &str, name: &str) -> bool {
    let prefix = format!("{}=ab-spawn-{name}-", config::ENV_SPAWN_TAG);
    let Some(rest) = tag.strip_prefix(&prefix) else {
        return false;
    };
    let Some((pid, hex)) = rest.rsplit_once('-') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && hex.len() == 12
        && hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// `cmd_ready`:1594 — worker 自報就緒；僅限 spawned agent。
pub fn ready(paths: &Paths, name: &str) -> Result<()> {
    with_spawned_registry(paths, name, "ready", |f| set_bool_field(f, "ready", true))?;
    let _ = writeln_stderr(&format!("agent '{name}' 已回報就緒（ready）"));
    Ok(())
}

/// `cmd_disposable`:1629 — worker 宣告「本輪脈絡已無殘值」。
///
/// 語意刻意是**單向宣告**而非雙向旗標：預設保留。沒宣告過的一律當成仍有殘值，
/// 失效方向因此安全——忘記宣告只是多佔一個 cap，而不是把有用的脈絡殺掉。
/// 一併寫 `disposable_at`，讓宣告能「過期」（見 `disposable_effective`）。
pub fn disposable(paths: &Paths, tmux: &dyn TmuxClient, name: &str) -> Result<()> {
    // pane/runtime 在鎖內取：出鎖後 registry 可能已被換掉，審計就記到別人頭上
    let (pane, runtime) = with_spawned_registry(paths, name, "disposable", |f| {
        let pane = registry::read_field(f, "pane_id", "-");
        let runtime = registry::read_field(f, "runtime", "-");
        let content = std::fs::read_to_string(f)
            .map_err(|e| Error::new(format!("無法讀取 {}：{e}", f.display())))?;
        let Ok(Value::Object(mut fields)) = json::parse(&content) else {
            return Err(Error::new(format!(
                "registry 檔 {} 不是合法的 JSON 物件",
                f.display()
            )));
        };
        fields.insert("disposable".into(), Value::Bool(true));
        json::set_str_field(&mut fields, "disposable_at", &now_iso());
        let doc = format!("{}\n", json::render_pretty(&Value::Object(fields)));
        atomic_write(f, doc.as_bytes())?;
        Ok((pane, runtime))
    })?;
    // 宣告已落進 registry，審計 append 失敗只揭露不翻盤
    if registry::log_agent_event(paths, tmux, "disposable", name, &pane, &runtime, None).is_err() {
        eprintln!(
            "agent-bridge: 警告：disposable 已寫入 registry，但審計未落地（agents.log append 失敗）"
        );
    }
    let _ = writeln_stderr(&format!("agent '{name}' 已宣告可回收（disposable）"));
    Ok(())
}

/// ready／disposable 共用的鎖內前置：名稱文法 → 取鎖 → 存在 → 出身三態。
/// 兩者的拒絕訊息只差一句尾巴，故以 `kind` 分流（逐字對齊 bash）。
fn with_spawned_registry<T>(
    paths: &Paths,
    name: &str,
    kind: &str,
    body: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    if !crate::validate::is_valid_name(name) {
        return Err(Error::new(format!(
            "agent 名稱不合法（僅允許 [A-Za-z0-9_-]+）：{name}"
        )));
    }
    let f = paths.agents_dir.join(format!("{name}.json"));
    let guard = acquire_lock(paths, "agents-registry")?;
    let outcome = (|| -> Result<T> {
        if !f.is_file() {
            return Err(Error::new(format!("未註冊的 agent：{name}")));
        }
        match registry::read_provenance(&f) {
            Provenance::Manual => Err(Error::new(if kind == "ready" {
                format!("agent '{name}' 非 spawn 出身，ready 僅限 spawned agent")
            } else {
                format!(
                    "agent '{name}' 非 spawn 出身，disposable 僅限 spawned agent（人工 pane 的生命週期不歸 bridge 管）"
                )
            })),
            Provenance::Undetermined => Err(Error::new(format!(
                "agent '{name}' 的 registry 無法解析，{kind} 拒絕；請確認 {} 後手動處理",
                f.display()
            ))),
            Provenance::Spawned => body(&f),
        }
    })();
    guard.release();
    outcome
}

fn set_bool_field(f: &Path, key: &str, val: bool) -> Result<()> {
    let content = std::fs::read_to_string(f)
        .map_err(|e| Error::new(format!("無法讀取 {}：{e}", f.display())))?;
    let Ok(Value::Object(mut fields)) = json::parse(&content) else {
        return Err(Error::new(format!(
            "registry 檔 {} 不是合法的 JSON 物件",
            f.display()
        )));
    };
    fields.insert(key.into(), Value::Bool(val));
    let doc = format!("{}\n", json::render_pretty(&Value::Object(fields)));
    atomic_write(f, doc.as_bytes())
}

/// `disposable_effective`:444 — 這個 agent 的「無殘值」宣告現在還算不算數。
///
/// **同秒也算過期**（`>=` 而非 `>`）：時間戳是秒精度，worker 宣告後
/// orchestrator 立刻派工就會落在同一秒。判太嚴只是多派一輪收尾，判太鬆是殺掉
/// 還有用的脈絡——兩個方向的代價不對等。
pub fn disposable_effective(paths: &Paths, name: &str, f: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(f) else {
        return false;
    };
    let Ok(Value::Object(fields)) = json::parse(&content) else {
        return false;
    };
    if !json::bool_field_is_true(&fields, "disposable") {
        return false;
    }
    // 宣告了但沒有時間戳（缺欄位／null → jq 給 `"?"`，或欄位本身是空字串）：
    // 無從判斷是否被後續任務推翻，當成無效（偏保守那邊）
    let disp_at = json::jq_alt(&fields, "disposable_at").unwrap_or_else(|| "?".to_string());
    if disp_at.is_empty() || disp_at == "?" {
        return false;
    }
    let last_at = task::last_task_at(paths, name);
    if last_at.is_empty() {
        return true;
    }
    last_at < disp_at
}

/// `cmd_idle`:1800 的一列。
pub struct IdleRow {
    pub name: String,
    pub ready: String,
    pub disposable: String,
    pub idle_secs: String,
}

/// `cmd_idle`:1800 — worker 池的回收決策視圖。**唯讀**：不取鎖、不寫任何檔案
/// （orchestrator 可能跑在只讀 sandbox 裡）。
///
/// `idle_secs` 取「最後任務」與「這個 pane 誕生」兩者較晚者：agent 名稱可以
/// 重用，而 `last_task_at` 只認名字不認 pane 實例——閒置時間不該早於誕生時間。
/// 無法解析時間時印 `-` 而不是 0（0 會被誤讀成「剛剛才用過」）。
pub fn idle(paths: &Paths) -> Vec<IdleRow> {
    let now = crate::time::now_epoch();
    let mut rows = Vec::new();
    for f in registry_files(paths) {
        let base_name = f
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let fields = match std::fs::read_to_string(&f).ok().map(|c| json::parse(&c)) {
            Some(Ok(Value::Object(m))) => m,
            // 損壞的 registry 不讓整份報表掛掉，但也不能靜靜跳過：它照樣佔著
            // cap，orchestrator 必須看得到「這裡有個判不出狀態的東西」
            _ => {
                rows.push(IdleRow {
                    name: base_name,
                    ready: "?".into(),
                    disposable: "?".into(),
                    idle_secs: "-".into(),
                });
                continue;
            }
        };
        let mut name = json::jq_raw_field(&fields, "name").unwrap_or_default();
        if name.is_empty() {
            name = base_name;
        }
        let spawned = json::bool_field_is_true(&fields, "spawned");
        let ready = if !spawned {
            "-"
        } else if json::bool_field_is_true(&fields, "ready") {
            "ready"
        } else {
            "starting"
        };
        // bash：`disp_at = if .disposable == true then (.disposable_at // "?") else ""`，
        // 再以 `[[ "$spawned" != y || -z "$disp_at" ]]` 判 `-`。
        // **`disposable_at` 是空字串時 jq 回空字串**（`//` 只對 null/false 生效），
        // 於是那一列印 `-` 而不是 `expired`——用 `jq_alt` 才拿得到這個區別。
        let disp_at = if json::bool_field_is_true(&fields, "disposable") {
            json::jq_alt(&fields, "disposable_at").unwrap_or_else(|| "?".to_string())
        } else {
            String::new()
        };
        // 人工註冊的 pane 不歸 bridge 管生命週期，一律 `-`：即使有人手動塞了
        // disposable 欄位，也不該讓它看起來可回收
        let disposable = if !spawned || disp_at.is_empty() {
            "-"
        } else if disposable_effective(paths, &name, &f) {
            "yes"
        } else {
            "expired"
        };

        // `.spawned_at // .registered_at // ""` 的鏈式 fallback，同樣要逐字：
        // `spawned_at: ""` 時 bash 停在空字串（idle_secs 印 `-`），不會往
        // registered_at 掉（codex 複核 2026-07-31）
        let reference = json::jq_alt(&fields, "spawned_at")
            .or_else(|| json::jq_alt(&fields, "registered_at"))
            .unwrap_or_default();
        let last_at = task::last_task_at(paths, &name);
        let base = if last_at.is_empty() || (!reference.is_empty() && reference > last_at) {
            reference
        } else {
            last_at
        };
        let idle_secs = match crate::time::parse_iso_to_epoch(&base) {
            Some(e) => (now - e).max(0).to_string(),
            None => "-".to_string(),
        };
        rows.push(IdleRow {
            name,
            ready: ready.into(),
            disposable: disposable.into(),
            idle_secs,
        });
    }
    rows
}

/// `list --long` 的一列（CLI-LIST-2）。
pub struct LongRow {
    pub name: String,
    pub pane: String,
    pub ready: String,
    pub origin: String,
    pub location: String,
    pub owner: String,
    pub disposable: String,
    pub idle_secs: String,
}

/// `list --long` 的欄名標頭。消費端跳過第一行（CLI-LIST-2）。
pub const LIST_LONG_HEADER: &str = "NAME\tPANE\tREADY\tORIGIN\tWHERE\tOWNER\tDISPOSABLE\tIDLE";

/// 索引查得到 → 位置字面值；查了但不在 → `dead_label`；沒得查 → `?`；
/// id 形狀不合法 → `invalid`。
///
/// **「查不到」與「沒得查」不可混為一談**：tmux 不可用時全池標 `?`（未知），
/// 誤標成 dead 會讓人以為整池該回收；反過來把真的死掉的標成未知，則讓
/// stale registry 看起來還活著。壞掉的 registry id（`@garbage`）是第三種事：
/// 那是資料損壞，標成 dead 會讓人以為「東西曾經在、現在沒了」。
///
/// **一個 id 可以對到多個位置**：tmux 的 window 可同時 linked 到多個 session
/// （`man tmux`「Windows may be linked to multiple sessions」），該 window 與
/// 其 panes 因此在 `-a` 列表出現多次。取最後一筆等於隨列序給答案——而使用者
/// 問的正是「跟哪個主 session 關聯」（跨廠複核 2026-07-31 的 major）。
/// `prefer_session` 給得出唯一配對時取那筆，否則全列出、逗號分隔，讓歧義
/// 顯形而不是被藏起來。
fn live_label(
    index: Option<&HashMap<String, Vec<String>>>,
    id: &str,
    dead_label: &str,
    prefer_session: Option<&str>,
) -> String {
    if !is_valid_pane(id) && !is_valid_window_id(id) {
        return "invalid".to_string();
    }
    let map = match index {
        None => return "?".to_string(),
        Some(m) => m,
    };
    let locs: Vec<&String> = match map.get(id) {
        Some(v) => v.iter().filter(|l| !l.is_empty()).collect(),
        None => return dead_label.to_string(),
    };
    match locs.len() {
        0 => dead_label.to_string(),
        1 => locs[0].clone(),
        _ => {
            // linked window：先用 registry 記的 session 名消歧義；配不出唯一
            // 的一筆就全列出（`a:1,b:1`），MUST NOT 任選一個
            if let Some(sess) = prefer_session {
                let matched: Vec<&&String> = locs
                    .iter()
                    .filter(|l| l.split(':').next() == Some(sess))
                    .collect();
                if matched.len() == 1 {
                    return matched[0].to_string();
                }
            }
            locs.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",")
        }
    }
}

/// `@<n>` 的形狀檢查，對映 `is_valid_pane` 的 `%<n>`。registry 的 id 是不可信
/// 輸入（人可手改），形狀不對就不該被當成「查得到／查不到」的問題。
fn is_valid_window_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    bytes.next() == Some(b'@') && {
        let rest: Vec<u8> = bytes.collect();
        !rest.is_empty() && rest.iter().all(|b| b.is_ascii_digit())
    }
}

/// registry 的 `owner` 欄形如 `<session>:@<winid>`，拆成（session 標籤,
/// window id）。**判定錨在不可變的 `@id`**；session 名可被 rename，只拿來在
/// linked window 多重位置時消歧義（CLI-LIST-2）。
fn owner_parts(owner: &str) -> Option<(&str, &str)> {
    let (sess, win) = owner.rsplit_once(':')?;
    if win.starts_with('@') && win.len() > 1 {
        Some((sess, win))
    } else {
        None
    }
}

/// 欄值不得含 TAB／換行／控制字元：輸出是「一 agent 一行、恰八欄」的 TSV，
/// 而 name／session 名等來自 registry 與 tmux，都是可被塞進怪字元的外部資料。
/// 合法 JSON string 帶一個 TAB 就能把一列變兩欄（跨廠複核 2026-07-31）。
fn sanitize_field(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() || c == '\t' { '_' } else { c })
        .collect()
}

/// 一次性索引：tmux 內部 id → 人看得懂的 `<session>:<window>`。
///
/// **不能用 `display -p -t <id>` 逐列查存在性**：tmux 對不存在的 window id
/// 會靜靜回 `:` 且 exit 0（實測 3.7b），單看 exit code 判不出死活，死掉的
/// owner 會被顯示成一個空位置。改用整行相等比對的列表（同 `pane_exists` 的
/// 既有作法），存在與否是決定性的；順帶把 N 次 exec 壓成一次。
///
/// 回傳 `None` ＝ tmux 沒得查（不可用／指令失敗），與「查了但不在」不同。
/// 值是 **`Vec`** 不是單值：linked window 讓同一 id 出現多次，用 map 覆寫會
/// 靜靜丟掉 cardinality（見 `live_label` 的說明）。排序去重讓輸出穩定。
fn tmux_index(
    tmux: &dyn TmuxClient,
    list_cmd: &str,
    id_fmt: &str,
) -> Option<HashMap<String, Vec<String>>> {
    let fmt = format!("{id_fmt}\t#{{session_name}}:#{{window_index}}");
    let out = tmux.exec(&[list_cmd, "-a", "-F", &fmt])?.ok_stdout()?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for line in out.lines() {
        if let Some((id, loc)) = line.split_once('\t') {
            map.entry(id.to_string())
                .or_default()
                .push(sanitize_field(loc));
        }
    }
    for v in map.values_mut() {
        v.sort();
        v.dedup();
    }
    Some(map)
}

/// `cmd_list --long`:CLI-LIST-2 — 人可介入的池視圖。**唯讀**：不取鎖、不寫檔、
/// 判定 dead 也不順手清 registry（回收一律走 despawn／evict 的顯式動作）。
///
/// 訊號與結論刻意分離：`origin` 是 provenance（manual 者 despawn 恆被拒），
/// `location`／`owner` 是 liveness，`disposable`／`idle_secs` 是 worker 自己
/// 留下的建議與閒置時間。沒有任何一欄是「可以安全刪除」——那是人的判斷。
pub fn list_long(paths: &Paths, tmux: &dyn TmuxClient) -> Vec<LongRow> {
    let panes = tmux_index(tmux, "list-panes", "#{pane_id}");
    let windows = tmux_index(tmux, "list-windows", "#{window_id}");
    let idle_rows: HashMap<String, IdleRow> = idle(paths)
        .into_iter()
        .map(|r| (r.name.clone(), r))
        .collect();

    let mut rows = Vec::new();
    for f in registry_files(paths) {
        let base_name = f
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // 損壞的 registry 照樣佔著 cap，必須看得見——整列 `?` 後繼續，
        // 不讓一個壞檔終止整份報表（同 idle 的既有處置）
        let fields = match std::fs::read_to_string(&f).ok().map(|c| json::parse(&c)) {
            Some(Ok(Value::Object(m))) => m,
            _ => {
                rows.push(LongRow {
                    name: base_name,
                    pane: "?".into(),
                    ready: "?".into(),
                    origin: "?".into(),
                    location: "?".into(),
                    owner: "?".into(),
                    disposable: "?".into(),
                    idle_secs: "-".into(),
                });
                continue;
            }
        };
        let mut name = json::jq_raw_field(&fields, "name").unwrap_or_default();
        if name.is_empty() {
            name = base_name;
        }
        let name = sanitize_field(&name);
        let pane = json::jq_raw_field(&fields, "pane_id").unwrap_or_default();
        let spawned = json::bool_field_is_true(&fields, "spawned");

        // owner 欄同時給出消歧義用的 session 標籤與判定用的 window id
        let owner_field = json::jq_raw_field(&fields, "owner").unwrap_or_default();
        let owner_bits = owner_parts(&owner_field);

        // WHERE 不用 owner 的 session 消歧義：pane 自己在哪、與誰派它出來是
        // 兩件事，registry 並沒有記 worker 自己的 session。linked window 下
        // 誠實的答案是「這兩個地方都是它」，全列出而不是挑一個看起來合理的
        let location = if pane.is_empty() {
            "?".to_string()
        } else {
            live_label(panes.as_ref(), &pane, "dead", None)
        };
        // 人工註冊者沒有 owner 概念：那個 pane 不歸 bridge 管生命週期
        let owner = match owner_bits {
            _ if !spawned => "-".to_string(),
            Some((sess, win)) => live_label(windows.as_ref(), win, "owner-dead", Some(sess)),
            None => "?".to_string(),
        };

        let (ready, disposable, idle_secs) = match idle_rows.get(&name) {
            Some(r) => (r.ready.clone(), r.disposable.clone(), r.idle_secs.clone()),
            None => ("?".to_string(), "?".to_string(), "-".to_string()),
        };
        rows.push(LongRow {
            name,
            pane: if pane.is_empty() {
                "-".into()
            } else {
                sanitize_field(&pane)
            },
            ready,
            origin: if spawned {
                "spawned".into()
            } else {
                "manual".into()
            },
            location,
            owner,
            disposable,
            idle_secs,
        });
    }
    rows
}

/// `for f in "$AGENTS_DIR"/*.json`（nullglob）：目錄缺失＝空集；排序對齊
/// bash glob 的字典序。
fn registry_files(paths: &Paths) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&paths.agents_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    files.sort();
    files
}

/// `[[ -f "$p" && -r "$p" ]]`：普通檔案且可讀。`-r` 對目錄一樣成立，故
/// `-f` 不可省（見 brief 預檢的理由）。
fn is_readable_file(p: &Path) -> bool {
    p.is_file() && std::fs::File::open(p).is_ok()
}

/// despawn 的兩處 `2>&1`：查詢失敗時把 tmux 的抱怨併進 die 訊息。
fn merged(o: &crate::tmux::TmuxOutput) -> String {
    format!("{}{}", o.stdout, o.stderr)
        .trim_end_matches('\n')
        .to_string()
}

/// `info()`:82 — 無前綴、走 stderr。寫不出去不翻盤（spawn 成功後的輸出失敗
/// 不該讓呼叫端以為失敗）。
fn writeln_stderr(s: &str) -> std::io::Result<()> {
    use std::io::Write;
    writeln!(std::io::stderr().lock(), "{s}")
}

fn writeln_stdout(s: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{s}")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CLI-LIST-2 的三態：查得到／查了不在／沒得查。三者混淆的代價不對稱——
    /// 把死的標成未知會讓 stale registry 看起來還活著，把未知標成死的會讓
    /// tmux 一不可用就整池看似該回收。
    #[test]
    fn live_label_separates_missing_from_unqueryable() {
        let m = HashMap::from([("%1".to_string(), vec!["scratch:3".to_string()])]);
        assert_eq!(live_label(Some(&m), "%1", "dead", None), "scratch:3");
        // 查了但不在 → dead 字面值
        assert_eq!(live_label(Some(&m), "%9", "dead", None), "dead");
        assert_eq!(live_label(Some(&m), "@9", "owner-dead", None), "owner-dead");
        // 沒得查（tmux 不可用／指令失敗）→ 未知，不是 dead
        assert_eq!(live_label(None, "%1", "dead", None), "?");
        // 索引裡存在但值為空不得當成活著
        let empty = HashMap::from([("%2".to_string(), vec![String::new()])]);
        assert_eq!(live_label(Some(&empty), "%2", "dead", None), "dead");
        // registry id 形狀壞掉是第三種事：資料損壞，不是「曾經在、現在沒了」
        assert_eq!(
            live_label(Some(&m), "@garbage", "owner-dead", None),
            "invalid"
        );
        assert_eq!(
            live_label(None, "%1 ; kill-server", "dead", None),
            "invalid"
        );
    }

    /// linked window：同一 id 對到多個 `<session>:<window>`。取最後一筆等於
    /// 隨列序給答案——而使用者問的正是「跟哪個主 session 關聯」
    /// （跨廠複核 2026-07-31 的 major）。
    #[test]
    fn live_label_never_picks_one_linked_location_arbitrarily() {
        let m = HashMap::from([(
            "@7".to_string(),
            vec!["alpha:1".to_string(), "beta:3".to_string()],
        )]);
        // 有 registry 記的 session 可消歧義 → 取那一筆
        assert_eq!(
            live_label(Some(&m), "@7", "owner-dead", Some("beta")),
            "beta:3"
        );
        // 配不出唯一的一筆 → 全列出，讓歧義顯形，MUST NOT 任選
        assert_eq!(
            live_label(Some(&m), "@7", "owner-dead", None),
            "alpha:1,beta:3"
        );
        assert_eq!(
            live_label(Some(&m), "@7", "owner-dead", Some("nosuch")),
            "alpha:1,beta:3"
        );
    }

    /// owner 欄形如 `<session>:@<winid>`；判定錨在不可變的 `@id`，
    /// session 名只用於 linked window 的消歧義（CLI-LIST-2）。
    #[test]
    fn owner_parts_split_label_from_immutable_id() {
        assert_eq!(owner_parts("scratch:@92"), Some(("scratch", "@92")));
        // session 名本身含冒號時取最後一個分隔點
        assert_eq!(owner_parts("a:b:@7"), Some(("a:b", "@7")));
        // 形狀不對一律不猜
        assert_eq!(owner_parts(""), None);
        assert_eq!(owner_parts("scratch"), None);
        assert_eq!(owner_parts("scratch:16"), None);
        assert_eq!(owner_parts("scratch:@"), None);
    }

    /// 欄值不得含 TAB／換行／控制字元：一個 TAB 就能把一列變成九欄。
    #[test]
    fn sanitize_field_protects_the_column_contract() {
        assert_eq!(sanitize_field("plain-name"), "plain-name");
        assert_eq!(sanitize_field("a\tb"), "a_b");
        assert_eq!(sanitize_field("a\nb\r\n"), "a_b__");
        assert_eq!(sanitize_field("bell\x07"), "bell_");
        // 空白與多位元組字元原樣保留（它們不破壞欄位邊界）
        assert_eq!(sanitize_field("有 空白"), "有 空白");
    }

    /// worker window 名不得逐代累積 `ab:` 前綴（使用者實測回報：現場堆到
    /// `ab:ab:ab:ab:ab:ab:ab:claude`）。累積來自 orchestrator 自己坐在
    /// `ab:` 開頭的 window（relay 接棒／從 worker window 內再 spawn）。
    #[test]
    fn worker_window_name_never_stacks_the_prefix() {
        assert_eq!(worker_window_name("claude"), "ab:claude");
        // 已帶前綴：剝掉再冠，不疊加
        assert_eq!(worker_window_name("ab:claude"), "ab:claude");
        assert_eq!(worker_window_name("ab:ab:ab:claude"), "ab:claude");
        // 空名／只剩前綴：退回 workers（與 owner_winname 為空時同解）
        assert_eq!(worker_window_name(""), "ab:workers");
        assert_eq!(worker_window_name("ab:"), "ab:workers");
        assert_eq!(worker_window_name("ab:ab:"), "ab:workers");
        // `ab` 不是前綴，不得被剝
        assert_eq!(worker_window_name("abc"), "ab:abc");
        assert_eq!(worker_window_name("ab"), "ab:ab");
    }

    /// `printf %q` 的安全字元集以本機 bash 實測導出；`,` 與空白會被反斜線
    /// 跳脫，測試 16a4／16a5 直接比對這個形狀。
    #[test]
    fn shell_quote_matches_bash_printf_q() {
        assert_eq!(shell_quote(b"http://sentinel:1"), "http://sentinel:1");
        assert_eq!(shell_quote(b"st a,b"), "st\\ a\\,b");
        assert_eq!(shell_quote(b"v 1,x"), "v\\ 1\\,x");
        assert_eq!(shell_quote(b""), "''");
        assert_eq!(shell_quote(b"a'b"), "a\\'b");
        assert_eq!(shell_quote(b"a\tb"), "$'a\\tb'");
        assert_eq!(shell_quote(b"a\x01b"), "$'a\\001b'");
        // `^` 不在安全集合內（實測 bash 5.3）
        assert_eq!(shell_quote(b"a^b"), "a\\^b");
        // UTF-8 多位元組在 UTF-8 locale 下原樣保留
        assert_eq!(shell_quote("ä".as_bytes()), "ä");
        // 非法 UTF-8 位元組必須無損地跳脫，不得變成 U+FFFD
        assert_eq!(shell_quote(&[b'a', 0xff, b'b']), "$'a\\377b'");
    }

    /// tag 格式是 despawn 的殺人許可：抄別人的 tag、或填一個萬用字串都得擋。
    #[test]
    fn tag_shape_binds_to_agent_name() {
        let ok = "AGENT_BRIDGE_SPAWN_TAG=ab-spawn-w1-12345-0123456789ab";
        assert!(tag_shape_ok(ok, "w1"));
        assert!(!tag_shape_ok(ok, "w2"), "名字不符必須擋");
        assert!(!tag_shape_ok("AGENT_BRIDGE_SPAWN_TAG=bash", "w1"));
        assert!(!tag_shape_ok("", "w1"));
        // hex 長度不足
        assert!(!tag_shape_ok(
            "AGENT_BRIDGE_SPAWN_TAG=ab-spawn-w1-12345-0123456789",
            "w1"
        ));
        // 大寫 hex 不在 [0-9a-f]
        assert!(!tag_shape_ok(
            "AGENT_BRIDGE_SPAWN_TAG=ab-spawn-w1-12345-0123456789AB",
            "w1"
        ));
    }

    /// 只回一個固定 `pane_pid` 的 tmux 替身，其餘子命令一律失敗。
    struct PidOnlyTmux(String);

    impl TmuxClient for PidOnlyTmux {
        fn exec(&self, args: &[&str]) -> Option<crate::tmux::TmuxOutput> {
            let ok = args.first() == Some(&"display-message") && args.contains(&"#{pane_pid}");
            Some(crate::tmux::TmuxOutput {
                status_ok: ok,
                stdout: if ok {
                    format!("{}\n", self.0)
                } else {
                    String::new()
                },
                stderr: String::new(),
            })
        }
        fn available(&self) -> bool {
            true
        }
        fn resolve_pane(&self, _target: &str) -> Option<String> {
            None
        }
        fn pane_exists(&self, _pane: &str) -> bool {
            false
        }
        fn capture_pane(&self, _pane: &str) -> Option<String> {
            None
        }
        fn pane_in_mode(&self, _pane: &str) -> Option<bool> {
            None
        }
        fn send_keys(&self, _pane: &str, _keys: &str) -> bool {
            false
        }
    }

    /// STATE-AGENT-4：`resolve_worker_identity` 必須真的驗過 argv 才記身分。
    ///
    /// 單元測試只驗 helper 正確擋不住這件事——把這裡的 attestation 整段拿掉，
    /// helper 的測試仍然全綠（codex 複核 2026-07-31 §4）。所以錨在 caller：
    /// pane_pid 指向一個 cmdline **不是** runtime 的行程（就用測試行程自己）
    /// 時，兩欄必須皆空。
    #[test]
    fn worker_identity_requires_runtime_attestation() {
        if !crate::proc::available() {
            return; // 非 Linux：這條沒有可觀測對象
        }
        let me = std::process::id().to_string();
        let tmux = PidOnlyTmux(me.clone());

        // 名字對不上 pane_pid 的 cmdline → 空欄 fallback，不得記下錯的 pid
        assert_eq!(
            resolve_worker_identity(&tmux, "%1", "__surely_not_a_runtime__"),
            (String::new(), String::new())
        );

        // 名字對得上時才記，且 starttime 必須是該 pid 當下的值
        let raw = std::fs::read(format!("/proc/{me}/cmdline")).unwrap();
        let argv0 = raw.split(|b| *b == 0).next().unwrap();
        let base =
            String::from_utf8(argv0.rsplit(|b| *b == b'/').next().unwrap().to_vec()).unwrap();
        let (pid, st) = resolve_worker_identity(&tmux, "%1", &base);
        assert_eq!(pid, me);
        assert_eq!(Some(st), crate::proc::starttime(&me));

        // tmux 拿不到 pane_pid → 空欄
        struct DeadTmux;
        impl TmuxClient for DeadTmux {
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
                false
            }
            fn capture_pane(&self, _p: &str) -> Option<String> {
                None
            }
            fn pane_in_mode(&self, _p: &str) -> Option<bool> {
                None
            }
            fn send_keys(&self, _p: &str, _k: &str) -> bool {
                false
            }
        }
        assert_eq!(
            resolve_worker_identity(&DeadTmux, "%1", &base),
            (String::new(), String::new())
        );
    }

    #[test]
    fn window_and_model_grammar() {
        assert!(is_valid_window("@3"));
        assert!(!is_valid_window("@"));
        assert!(!is_valid_window("%3"));
        assert!(is_valid_model("sonnet-t.0"));
        assert!(!is_valid_model("x;kill-server"));
        assert!(!is_valid_model("x y"));
        assert!(!is_valid_model("--bare"));
        assert!(!is_valid_model(""));
        assert!(!is_valid_model(&"a".repeat(65)));
        assert!(is_valid_model(&"a".repeat(64)));
    }
}
