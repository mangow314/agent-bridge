//! ab-host：把一個 ACP adapter（claude-agent-acp／codex-acp）接到 agent-bridge
//! mailbox。輪詢 `queued` 且 `to == name` 的 task，經 `session/prompt` 餵給
//! adapter，把結果直接以 ab-core 寫回 `response.md`＋終態（不呼叫 `agent-bridge
//! reply`／`fail`，故不做 caller 綁定檢查、也不通知 sender，sender 以 await 輪詢）。
//!
//! 整個 host 迴圈跑在 `connect_with` 的閉包內（該 future 同時驅動 JSON-RPC
//! dispatch），因此閉包內只用 `async_io::Timer` 睡眠，不可 `thread::sleep`。

use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ab_core::error::Result as CoreResult;
use ab_core::lock::acquire_lock;
use ab_core::paths::Paths;
use ab_core::task::{self, MessageSource, TaskState};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AuthenticateRequest, CancelNotification, ContentBlock, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind, PromptRequest,
    PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, SetSessionModeRequest,
    StopReason, TextContent, ToolKind,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, ConnectionTo, JsonRpcRequest, JsonRpcResponse,
};
use async_io::Timer;

const DEFAULT_PROMPT_TIMEOUT_SECS: u64 = 600;

// ---------------------------------------------------------------- 純邏輯

/// 權限決策結果。`NoOption`＝該走的 option 種類一個都沒有：`Selected` 需要
/// optionId，此時唯一合法的回應是 `Cancelled`。
#[derive(Debug, PartialEq, Eq)]
enum Choice<I> {
    Allow(I),
    Reject(I),
    NoOption,
}

impl<I> Choice<I> {
    /// events.log 的 decision 欄位值。
    fn label(&self) -> &'static str {
        match self {
            Choice::Allow(_) => "allow",
            Choice::Reject(_) | Choice::NoOption => "reject",
        }
    }
}

fn first_of<I>(opts: &[(I, PermissionOptionKind)], kinds: [PermissionOptionKind; 2]) -> Option<&I> {
    kinds
        .iter()
        .find_map(|k| opts.iter().find(|(_, ok)| ok == k).map(|(id, _)| id))
}

/// think → allow_once（無則 allow_always）；read／search 僅在 `in_cwd`
/// （locations 非空且全在 --cwd 內）時同上；其餘（含缺欄位）→ reject_once
/// （無則 reject_always）。
fn choose_permission<'a, I>(
    kind: Option<&ToolKind>,
    in_cwd: bool,
    opts: &'a [(I, PermissionOptionKind)],
) -> Choice<&'a I> {
    let safe = match kind {
        Some(ToolKind::Think) => true,
        Some(ToolKind::Read | ToolKind::Search) => in_cwd,
        _ => false,
    };
    if safe {
        match first_of(
            opts,
            [
                PermissionOptionKind::AllowOnce,
                PermissionOptionKind::AllowAlways,
            ],
        ) {
            Some(id) => Choice::Allow(id),
            None => Choice::NoOption,
        }
    } else {
        match first_of(
            opts,
            [
                PermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways,
            ],
        ) {
            Some(id) => Choice::Reject(id),
            None => Choice::NoOption,
        }
    }
}

/// 逐段處理 `.`／`..`（不碰檔案系統）。
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 能 canonicalize 就用它（解開 symlink）；否則 absolute＋逐段處理 `..`，
/// 再把最深的既存祖先 canonicalize，其餘段落接回去。
fn resolve_path(p: &Path) -> Option<PathBuf> {
    if let Ok(c) = p.canonicalize() {
        return Some(c);
    }
    let norm = lexical_normalize(&std::path::absolute(p).ok()?);
    let mut rest = Vec::new();
    let mut cur = norm.as_path();
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            out.extend(rest.iter().rev());
            return Some(out);
        }
        rest.push(cur.file_name()?.to_owned());
        cur = cur.parent()?;
    }
}

/// locations 非空且每個 path 解析後都在 `cwd` 底下才為 true。
fn locations_in_cwd(paths: &[PathBuf], cwd: &Path) -> bool {
    if paths.is_empty() {
        return false;
    }
    let Some(root) = resolve_path(cwd) else {
        return false;
    };
    paths
        .iter()
        .all(|p| resolve_path(p).is_some_and(|r| r.starts_with(&root)))
}

/// session/new 之後的 mode 判定：Ok(None)＝現值已合規，Ok(Some(m))＝要 set 成 m，
/// Err＝拒絕啟動（原因）。
fn decide_mode(
    runtime: &str,
    current: &str,
    available: &[&str],
) -> Result<Option<&'static str>, String> {
    if runtime == "claude" {
        let target = "default";
        if !available.contains(&target) {
            return Err(format!("availableModes 不含 {target}：{available:?}"));
        }
        Ok((current != target).then_some(target))
    } else {
        const ALLOWED: [&str; 2] = ["read-only", "workspace-write"];
        if ALLOWED.contains(&current) {
            Ok(None)
        } else {
            Err(format!(
                "codex 現行 mode 不在允許集合 {ALLOWED:?}：{current}"
            ))
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Terminal {
    Complete,
    Fail(String),
    Cancelled,
}

/// stopReason → task 終態。`end_turn` 完成、`cancelled` 交給取消路徑，其餘
/// （refusal／max_tokens／max_turn_requests 與未來新增值）一律 fail。
fn map_stop(r: &StopReason) -> Terminal {
    match r {
        StopReason::EndTurn => Terminal::Complete,
        StopReason::Cancelled => Terminal::Cancelled,
        other => {
            let name = serde_json::to_value(other)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{other:?}"));
            Terminal::Fail(format!("stopReason={name}"))
        }
    }
}

fn kind_str(k: Option<&ToolKind>) -> &'static str {
    match k {
        None => "unknown",
        Some(ToolKind::Read) => "read",
        Some(ToolKind::Edit) => "edit",
        Some(ToolKind::Delete) => "delete",
        Some(ToolKind::Move) => "move",
        Some(ToolKind::Search) => "search",
        Some(ToolKind::Execute) => "execute",
        Some(ToolKind::Think) => "think",
        Some(ToolKind::Fetch) => "fetch",
        Some(ToolKind::SwitchMode) => "switch_mode",
        Some(_) => "other",
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

// ---------------------------------------------------------------- 狀態轉換

/// 取 per-task 鎖，status 在 `accept` 內才轉態並記事件。回 false＝status 已不在
/// 可接受集合（例如已被 cancel），呼叫端不得再動這個 task。
fn transition(
    paths: &Paths,
    id: &str,
    accept: &[&str],
    to: TaskState,
    event: &str,
    detail: &str,
) -> CoreResult<bool> {
    let dir = task::require_task_dir(paths, id)?;
    let guard = acquire_lock(paths, id)?;
    let out = (|| -> CoreResult<bool> {
        let st = task::read_status(&dir)?;
        if !accept.contains(&st.as_str()) {
            return Ok(false);
        }
        task::update_meta_status(&dir, to)?;
        task::log_event(paths, id, event, detail)?;
        Ok(true)
    })();
    guard.release();
    out
}

/// reply／fail 的 host 版：delivered/running → 寫 response.md → 終態 → 事件。
fn finish(
    paths: &Paths,
    id: &str,
    to: TaskState,
    event: &str,
    detail: &str,
    body: &[u8],
) -> CoreResult<bool> {
    let dir = task::require_task_dir(paths, id)?;
    let guard = acquire_lock(paths, id)?;
    let out = (|| -> CoreResult<bool> {
        let st = task::read_status(&dir)?;
        if st != "delivered" && st != "running" {
            return Ok(false);
        }
        task::write_message(
            &dir.join("response.md"),
            &MessageSource::Text(body.to_vec()),
        )?;
        task::update_meta_status(&dir, to)?;
        task::log_event(paths, id, event, detail)?;
        Ok(true)
    })();
    guard.release();
    out
}

fn fail_task(paths: &Paths, id: &str, reason: &str) -> CoreResult<bool> {
    finish(
        paths,
        id,
        TaskState::Failed,
        "failed",
        &format!("reason={}", one_line(reason)),
        reason.as_bytes(),
    )
}

/// 最舊的 queued 且 to==name（`in_flight` 已依 task-id＝時間序排序）。
fn next_queued(paths: &Paths, name: &str) -> Option<String> {
    task::in_flight(paths)
        .into_iter()
        .find(|t| t.to == name && t.status == "queued")
        .map(|t| t.id)
}

// ---------------------------------------------------------------- 共享狀態

#[derive(Default)]
struct State {
    task: Option<String>,
    text: String,
    cancel: bool,
}

type Shared = Arc<Mutex<State>>;
type Slot<T> = Arc<Mutex<Option<Result<T, String>>>>;

fn lock_state(s: &Shared) -> std::sync::MutexGuard<'_, State> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

/// 送出 request，回應（含錯誤）落進 slot。callback 恆回 Ok：回 Err 會終止連線。
fn dispatch<Req>(
    cx: &ConnectionTo<Agent>,
    req: Req,
) -> Result<Slot<Req::Response>, agent_client_protocol::Error>
where
    Req: JsonRpcRequest,
    Req::Response: JsonRpcResponse + Send + 'static,
{
    let slot: Slot<Req::Response> = Arc::new(Mutex::new(None));
    let s2 = slot.clone();
    cx.send_request(req).on_receiving_result(async move |res| {
        *s2.lock().unwrap_or_else(|e| e.into_inner()) = Some(res.map_err(|e| e.to_string()));
        Ok(())
    })?;
    Ok(slot)
}

fn take<T>(slot: &Slot<T>) -> Option<Result<T, String>> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).take()
}

async fn call<Req>(
    cx: &ConnectionTo<Agent>,
    req: Req,
    timeout: Duration,
    what: &str,
) -> Result<Req::Response, String>
where
    Req: JsonRpcRequest,
    Req::Response: JsonRpcResponse + Send + 'static,
{
    let slot = dispatch(cx, req).map_err(|e| format!("{what}: {e}"))?;
    let start = Instant::now();
    loop {
        if let Some(r) = take(&slot) {
            return r.map_err(|e| format!("{what}: {e}"));
        }
        if start.elapsed() >= timeout {
            return Err(format!("{what}: timeout after {}s", timeout.as_secs()));
        }
        Timer::after(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------- CLI

struct Opts {
    runtime: String,
    name: String,
    cwd: PathBuf,
    data: Option<PathBuf>,
}

fn parse_args() -> Result<Opts, String> {
    let usage = "用法：ab-host --runtime claude|codex --name <name> [--cwd <dir>] [--data <dir>]";
    let mut runtime = None;
    let mut name = None;
    let mut cwd = None;
    let mut data = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} 缺值\n{usage}"));
        match a.as_str() {
            "--runtime" => runtime = Some(val()?),
            "--name" => name = Some(val()?),
            "--cwd" => cwd = Some(PathBuf::from(val()?)),
            "--data" => data = Some(PathBuf::from(val()?)),
            _ => return Err(format!("未知參數：{a}\n{usage}")),
        }
    }
    let runtime = runtime.ok_or(usage)?;
    if runtime != "claude" && runtime != "codex" {
        return Err(format!("--runtime 只接受 claude|codex：{runtime}"));
    }
    let name = name.ok_or(usage)?;
    let cwd = match cwd {
        Some(c) => c,
        None => std::env::current_dir().map_err(|e| format!("無法取得 cwd：{e}"))?,
    };
    let cwd = std::path::absolute(&cwd).map_err(|e| format!("--cwd 無效：{e}"))?;
    Ok(Opts {
        runtime,
        name,
        cwd,
        data,
    })
}

fn poll_interval() -> Result<Duration, String> {
    let raw = std::env::var(ab_core::config::ENV_POLL_INTERVAL).unwrap_or_default();
    if raw.is_empty() {
        return Ok(Duration::from_secs(1));
    }
    match raw.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Duration::try_from_secs_f64(v)
            .map_err(|_| format!("AGENT_BRIDGE_POLL_INTERVAL 超出範圍：{raw}")),
        _ => Err(format!("AGENT_BRIDGE_POLL_INTERVAL 需為正數（秒）：{raw}")),
    }
}

fn prompt_timeout() -> Result<Duration, String> {
    let raw = std::env::var("AB_HOST_PROMPT_TIMEOUT").unwrap_or_default();
    if raw.is_empty() {
        return Ok(Duration::from_secs(DEFAULT_PROMPT_TIMEOUT_SECS));
    }
    match raw.parse::<u64>() {
        Ok(v) if v > 0 => Ok(Duration::from_secs(v)),
        _ => Err(format!("AB_HOST_PROMPT_TIMEOUT 需為正整數（秒）：{raw}")),
    }
}

fn adapter(runtime: &str) -> AcpAgent {
    // 與 026 探針同形：unset API key 環境變數，走訂閱登入（Max／ChatGPT）。
    let cfg = if runtime == "claude" {
        AcpAgentConfig::new("env").args([
            "-u",
            "ANTHROPIC_API_KEY",
            "npx",
            "-y",
            "@agentclientprotocol/claude-agent-acp@0.88.0",
        ])
    } else {
        AcpAgentConfig::new("env")
            .args([
                "-u",
                "OPENAI_API_KEY",
                "-u",
                "CODEX_API_KEY",
                "npx",
                "-y",
                "@agentclientprotocol/codex-acp@2.1.1",
            ])
            .env("INITIAL_AGENT_MODE", "workspace-write")
            .env("NO_BROWSER", "1")
    };
    AcpAgent::new(cfg)
}

fn flush_stdout() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

// ---------------------------------------------------------------- 主流程

fn run(opts: Opts) -> Result<(), String> {
    let mut paths = Paths::resolve();
    if let Some(d) = &opts.data {
        let d = std::path::absolute(d).map_err(|e| format!("--data 無效：{e}"))?;
        paths = Paths {
            agents_dir: d.join("agents"),
            tasks_dir: d.join("tasks"),
            locks_dir: d.join("locks"),
            state_dir: d.join("state"),
            data_dir: d,
        };
    }
    paths.ensure_dirs().map_err(|e| e.to_string())?;
    let poll = poll_interval()?;
    let ptimeout = prompt_timeout()?;

    // 啟動掃描：本 host 上次的 session 已死，in-flight 的 task 不可能再完成。
    for t in task::in_flight(&paths) {
        if t.to == opts.name && (t.status == "running" || t.status == "delivered") {
            match fail_task(&paths, &t.id, "host restarted; session lost") {
                Ok(true) => eprintln!("ab-host: task {} 標記 failed（host restarted）", t.id),
                Ok(false) => {}
                Err(e) => return Err(format!("無法回收殘留 task {}：{e}", t.id)),
            }
        }
    }

    let shared: Shared = Arc::new(Mutex::new(State::default()));

    let sh_n = shared.clone();
    let sh_p = shared.clone();
    let paths_p = paths.clone();
    let name = opts.name.clone();
    let cwd = opts.cwd.clone();
    let cwd_p = opts.cwd.clone();
    let runtime = opts.runtime.clone();

    let result = async_io::block_on(
        agent_client_protocol::Client
            .builder()
            .on_receive_notification(
                async move |n: SessionNotification, _cx| {
                    match n.update {
                        SessionUpdate::AgentMessageChunk(c) => {
                            if let ContentBlock::Text(t) = c.content {
                                lock_state(&sh_n).text.push_str(&t.text);
                                print!("{}", t.text);
                                flush_stdout();
                            }
                        }
                        SessionUpdate::ToolCall(tc) => {
                            println!(
                                "\n[tool] {} kind={} status={:?}",
                                one_line(&tc.title),
                                kind_str(Some(&tc.kind)),
                                tc.status
                            );
                        }
                        SessionUpdate::ToolCallUpdate(u) => {
                            let f = &u.fields;
                            println!(
                                "[tool-update] {} status={}",
                                f.title.as_deref().map(one_line).unwrap_or_default(),
                                f.status
                                    .as_ref()
                                    .map(|s| format!("{s:?}"))
                                    .unwrap_or_default()
                            );
                        }
                        _ => {}
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |req: RequestPermissionRequest, responder, _cx| {
                    let kind = req.tool_call.fields.kind.as_ref();
                    let title = req
                        .tool_call
                        .fields
                        .title
                        .as_deref()
                        .map(one_line)
                        .unwrap_or_default();
                    let opts: Vec<(_, PermissionOptionKind)> = req
                        .options
                        .iter()
                        .map(|o: &PermissionOption| (o.option_id.clone(), o.kind))
                        .collect();
                    let (cancelled, task_id) = {
                        let s = lock_state(&sh_p);
                        (s.cancel, s.task.clone())
                    };
                    eprintln!(
                        "ab-host: request_permission kind={} title={title}",
                        kind_str(kind)
                    );
                    if cancelled {
                        // 取消進行中：立即回應的策略下不會有 pending，這裡處理
                        // 取消期間才到的請求。
                        if let Some(id) = &task_id {
                            let _ = task::log_event(
                                &paths_p,
                                id,
                                "permission",
                                &format!(
                                    "kind={} title={title} decision=cancelled",
                                    kind_str(kind)
                                ),
                            );
                        }
                        return responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ));
                    }
                    let locs: Vec<PathBuf> = req
                        .tool_call
                        .fields
                        .locations
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .map(|l| l.path.clone())
                        .collect();
                    let in_cwd = locations_in_cwd(&locs, &cwd_p);
                    let choice = choose_permission(kind, in_cwd, &opts);
                    if let Some(id) = &task_id
                        && let Err(e) = task::log_event(
                            &paths_p,
                            id,
                            "permission",
                            &format!(
                                "kind={} title={title} decision={}",
                                kind_str(kind),
                                choice.label()
                            ),
                        )
                    {
                        eprintln!("ab-host: 警告：permission 事件寫入失敗：{e}");
                    }
                    let outcome = match choice {
                        Choice::Allow(id) | Choice::Reject(id) => {
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                id.clone(),
                            ))
                        }
                        Choice::NoOption => RequestPermissionOutcome::Cancelled,
                    };
                    responder.respond(RequestPermissionResponse::new(outcome))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(adapter(&opts.runtime), {
                let shared = shared.clone();
                move |cx: ConnectionTo<Agent>| async move {
                    host_loop(cx, paths, name, cwd, runtime, shared, poll, ptimeout)
                        .await
                        .map_err(agent_client_protocol::util::internal_error)
                }
            }),
    );
    result.map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn host_loop(
    cx: ConnectionTo<Agent>,
    paths: Paths,
    name: String,
    cwd: PathBuf,
    runtime: String,
    shared: Shared,
    poll: Duration,
    ptimeout: Duration,
) -> Result<(), String> {
    // ---- initialize → (必要時 authenticate) → session/new
    let init: InitializeResponse = call(
        &cx,
        InitializeRequest::new(ProtocolVersion::V1),
        ptimeout,
        "initialize",
    )
    .await?;
    let new_req = || NewSessionRequest::new(cwd.clone());
    let sess: NewSessionResponse = match call(&cx, new_req(), ptimeout, "session/new").await {
        Ok(s) => s,
        Err(first) => {
            let mut got = None;
            for m in &init.auth_methods {
                call(
                    &cx,
                    AuthenticateRequest::new(m.id().clone()),
                    ptimeout,
                    "authenticate",
                )
                .await?;
                if let Ok(s) = call(&cx, new_req(), ptimeout, "session/new").await {
                    got = Some(s);
                    break;
                }
            }
            got.ok_or(first)?
        }
    };
    let session_id = sess.session_id;
    // fail-closed：adapter 預設 mode（如 claude 的 "auto"）會自行放行寫檔而不發
    // request_permission，使權限策略失效；mode 不合規或無法確認一律拒絕啟動。
    // set_mode 的 Ok 回應即為 adapter 對切換的確認（回應本身不帶 mode）。
    let refuse = |why: String| {
        // 連線層會把錯誤包成 internal error，故在此直接印出約定格式。
        let msg = format!("拒絕啟動：{why}");
        eprintln!("ab-host: {msg}");
        msg
    };
    let modes = sess
        .modes
        .as_ref()
        .ok_or_else(|| refuse("session/new 未回報 modes".into()))?;
    let available: Vec<&str> = modes
        .available_modes
        .iter()
        .map(|a| a.id.0.as_ref())
        .collect();
    if let Some(target) =
        decide_mode(&runtime, modes.current_mode_id.0.as_ref(), &available).map_err(refuse)?
    {
        call(
            &cx,
            SetSessionModeRequest::new(session_id.clone(), target),
            ptimeout,
            "session/set_mode",
        )
        .await
        .map_err(|e| refuse(format!("set_mode 失敗：{e}")))?;
    }
    eprintln!("ab-host: ready name={name} session={session_id:?}");

    // ---- mailbox 迴圈
    loop {
        let Some(id) = next_queued(&paths, &name) else {
            Timer::after(poll).await;
            continue;
        };
        if !transition(
            &paths,
            &id,
            &["queued"],
            TaskState::Delivered,
            "delivered",
            "",
        )
        .map_err(|e| e.to_string())?
        {
            continue;
        }
        let dir = task::task_dir(&paths, &id);
        let bytes = std::fs::read(dir.join("request.md"))
            .map_err(|e| format!("無法讀取 {id}/request.md：{e}"))?;
        let prompt = String::from_utf8_lossy(&bytes).into_owned();
        {
            let mut s = lock_state(&shared);
            s.task = Some(id.clone());
            s.text.clear();
            s.cancel = false;
        }
        eprintln!("ab-host: task {id} → session/prompt");
        let slot = dispatch(
            &cx,
            PromptRequest::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new(prompt))],
            ),
        )
        .map_err(|e| e.to_string())?;
        // 送出後才轉 running（delivered → prompt → running 的順序）
        let still = transition(
            &paths,
            &id,
            &["delivered"],
            TaskState::Running,
            "started",
            "",
        )
        .map_err(|e| e.to_string())?;
        let started = Instant::now();
        let mut cancel_sent = false;
        if !still {
            // 在 delivered 與 running 之間被 cancel：prompt 已送出，立刻取消
            cx.send_notification(CancelNotification::new(session_id.clone()))
                .map_err(|e| e.to_string())?;
            lock_state(&shared).cancel = true;
            cancel_sent = true;
        }

        let resp: Result<PromptResponse, String> = loop {
            if let Some(r) = take(&slot) {
                break r;
            }
            if started.elapsed() >= ptimeout {
                let _ = fail_task(&paths, &id, "prompt timeout").map_err(|e| e.to_string())?;
                lock_state(&shared).task = None;
                return Err(format!(
                    "task {id} prompt timeout（{}s）；adapter session 狀態未知，host 結束",
                    ptimeout.as_secs()
                ));
            }
            Timer::after(poll.min(Duration::from_millis(500))).await;
            if !cancel_sent
                && task::read_status(&dir)
                    .map(|s| s == "cancelled")
                    .unwrap_or(false)
            {
                cx.send_notification(CancelNotification::new(session_id.clone()))
                    .map_err(|e| e.to_string())?;
                lock_state(&shared).cancel = true;
                cancel_sent = true;
                eprintln!("ab-host: task {id} cancelled → session/cancel");
            }
        };

        let text = std::mem::take(&mut lock_state(&shared).text);
        println!();
        match resp {
            Err(e) => {
                fail_task(&paths, &id, &format!("prompt error: {e}")).map_err(|e| e.to_string())?;
            }
            Ok(r) => match map_stop(&r.stop_reason) {
                Terminal::Complete => {
                    finish(
                        &paths,
                        &id,
                        TaskState::Completed,
                        "replied",
                        "",
                        text.as_bytes(),
                    )
                    .map_err(|e| e.to_string())?;
                }
                Terminal::Fail(reason) => {
                    fail_task(&paths, &id, &reason).map_err(|e| e.to_string())?;
                }
                Terminal::Cancelled => {
                    // status 已由 `cancel` 指令寫成 cancelled，host 不再動它
                    eprintln!("stopReason=cancelled");
                }
            },
        }
        let mut s = lock_state(&shared);
        s.task = None;
        s.cancel = false;
    }
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("ab-host: {e}");
            return ExitCode::from(2);
        }
    };
    match run(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ab-host: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PermissionOptionKind::*;

    fn opts(kinds: &[PermissionOptionKind]) -> Vec<(&'static str, PermissionOptionKind)> {
        let ids = ["o0", "o1", "o2", "o3"];
        kinds
            .iter()
            .enumerate()
            .map(|(i, k)| (ids[i], *k))
            .collect()
    }

    #[test]
    fn safe_kinds_pick_allow_once_then_allow_always() {
        for k in [ToolKind::Read, ToolKind::Search, ToolKind::Think] {
            let o = opts(&[AllowAlways, RejectOnce, AllowOnce]);
            assert_eq!(choose_permission(Some(&k), true, &o), Choice::Allow(&"o2"));
            let o = opts(&[RejectOnce, AllowAlways]);
            assert_eq!(choose_permission(Some(&k), true, &o), Choice::Allow(&"o1"));
        }
    }

    #[test]
    fn other_kinds_and_missing_kind_pick_reject() {
        for k in [
            Some(ToolKind::Edit),
            Some(ToolKind::Execute),
            Some(ToolKind::Fetch),
            Some(ToolKind::Other),
            None,
        ] {
            let o = opts(&[AllowOnce, RejectAlways, RejectOnce]);
            assert_eq!(
                choose_permission(k.as_ref(), true, &o),
                Choice::Reject(&"o2")
            );
            let o = opts(&[AllowOnce, RejectAlways]);
            assert_eq!(
                choose_permission(k.as_ref(), true, &o),
                Choice::Reject(&"o1")
            );
        }
    }

    #[test]
    fn no_matching_option_is_noption_and_labels_reject() {
        let o = opts(&[RejectOnce]);
        let c = choose_permission(Some(&ToolKind::Read), true, &o);
        assert_eq!(c, Choice::NoOption);
        assert_eq!(c.label(), "reject");
        let o = opts(&[AllowOnce]);
        assert_eq!(
            choose_permission(Some(&ToolKind::Edit), true, &o),
            Choice::NoOption
        );
        assert_eq!(
            choose_permission::<&str>(Some(&ToolKind::Read), true, &[]),
            Choice::NoOption
        );
        assert_eq!(Choice::Allow(&"x").label(), "allow");
        assert_eq!(Choice::Reject(&"x").label(), "reject");
    }

    #[test]
    fn stop_reason_maps_to_terminal() {
        assert_eq!(map_stop(&StopReason::EndTurn), Terminal::Complete);
        assert_eq!(map_stop(&StopReason::Cancelled), Terminal::Cancelled);
        assert_eq!(
            map_stop(&StopReason::Refusal),
            Terminal::Fail("stopReason=refusal".into())
        );
        assert_eq!(
            map_stop(&StopReason::MaxTokens),
            Terminal::Fail("stopReason=max_tokens".into())
        );
        assert_eq!(
            map_stop(&StopReason::MaxTurnRequests),
            Terminal::Fail("stopReason=max_turn_requests".into())
        );
    }

    #[test]
    fn read_search_need_in_cwd_but_think_does_not() {
        let o = opts(&[AllowOnce, RejectOnce]);
        for k in [ToolKind::Read, ToolKind::Search] {
            assert_eq!(
                choose_permission(Some(&k), false, &o),
                Choice::Reject(&"o1")
            );
        }
        assert_eq!(
            choose_permission(Some(&ToolKind::Think), false, &o),
            Choice::Allow(&"o0")
        );
    }

    #[test]
    fn locations_scoped_to_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(cwd.join("a.txt"), "a").unwrap();
        std::fs::write(outside.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(&outside, cwd.join("link")).unwrap();

        // cwd 內（既存、不存在）→ true
        assert!(locations_in_cwd(&[cwd.join("a.txt")], &cwd));
        assert!(locations_in_cwd(&[cwd.join("new/none.txt")], &cwd));
        // cwd 外 → false
        assert!(!locations_in_cwd(&[outside.join("secret")], &cwd));
        // `..` 跳出（既存與不存在）→ false
        assert!(!locations_in_cwd(&[cwd.join("../outside/secret")], &cwd));
        assert!(!locations_in_cwd(&[cwd.join("../nope")], &cwd));
        // locations 空 → false；任一在外 → false
        assert!(!locations_in_cwd(&[], &cwd));
        assert!(!locations_in_cwd(
            &[cwd.join("a.txt"), outside.join("secret")],
            &cwd
        ));
        // cwd 內 symlink 指向 cwd 外 → false（既存與不存在的子路徑）
        assert!(!locations_in_cwd(&[cwd.join("link/secret")], &cwd));
        assert!(!locations_in_cwd(&[cwd.join("link/missing")], &cwd));
    }

    #[test]
    fn mode_decision_claude() {
        let av = ["auto", "default", "plan"];
        assert_eq!(decide_mode("claude", "default", &av), Ok(None));
        assert_eq!(decide_mode("claude", "auto", &av), Ok(Some("default")));
        assert!(decide_mode("claude", "auto", &["auto", "plan"]).is_err());
    }

    #[test]
    fn mode_decision_codex() {
        let av = ["read-only", "workspace-write", "agent", "agent-full-access"];
        assert_eq!(decide_mode("codex", "read-only", &av), Ok(None));
        assert_eq!(decide_mode("codex", "workspace-write", &av), Ok(None));
        assert!(decide_mode("codex", "agent-full-access", &av).is_err());
        assert!(decide_mode("codex", "agent", &av).is_err());
    }
}
