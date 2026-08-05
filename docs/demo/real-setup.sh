#!/usr/bin/env bash
# 真 agent hero demo 錄影環境（real-hero.tape 的 Hidden 階段呼叫，也可手動
# 彩排）。與 stub 版 setup.sh 的差別：不裝 codex shim、不跑 driver——
# orchestrator pane 跑真 Claude Code，worker 是 spawn 就地彈出的真 codex，
# 兩個 agent 都活的、會打真實 API。可重複執行（先殺舊 server、砍舊資料目錄；
# ROOT 路徑固定，兩層信任記錄都以路徑為鍵跨次有效）。
# 前置（各一次、人互動授予）：claude 的資料夾信任框，與 codex 的目錄信任框
# （否則 spawn 的 ready 探針會打進對話框、worker 直接退出——2026-08-05 彩排
# 實測教訓；後者記在 ~/.codex/agent-worker.config.toml 的 [projects] 區）。
# 畫面瑕疵坑：若環境同時有 claude.ai 登入與 ANTHROPIC_API_KEY，錄影裡的
# claude 開頭會印 auth 警示橫幅（discussion take 1 實錄）。錄影 pane 一律
# env -u 拿掉 key：與 hero take 一致走 claude.ai 登入，畫面乾淨；要改走
# API key 計費就把下面 env -u 拿掉並確保未登入。
set -euo pipefail

SOCK=ab-real
ROOT="${REAL_DEMO_ROOT:-${TMPDIR:-/tmp}/agent-bridge-real-demo}"
REPO="$(cd "$(dirname "$0")/../.." && pwd)"

tmux -L "$SOCK" kill-server 2>/dev/null || true

# 砍 ROOT 前的防呆（codex 審查 2026-08-05）：REAL_DEMO_ROOT 是使用者可控
# 輸入，必須含專用 basename 才准遞迴刪除，擋掉誤設 $HOME／repo／/ 整棵刪
case "$ROOT" in
  */agent-bridge-real-demo*) ;;
  *) echo "拒絕清除 ROOT=$ROOT（路徑須含 agent-bridge-real-demo）" >&2; exit 1 ;;
esac
rm -rf -- "$ROOT"
mkdir -p "$ROOT/shim" "$ROOT/data" "$ROOT/project/docs" "$ROOT/project/.claude"

# 釘住 repo 版 agent-bridge；claude／codex 用使用者 PATH 上的真品
ln -s "$REPO/bin/agent-bridge" "$ROOT/shim/agent-bridge"

# scratch 專案：小而真的任務素材，worker 讀它回一行摘要
cat > "$ROOT/project/docs/notes.md" <<'MD'
# agent-bridge 0.4 — release notes draft

## Highlights

- `spawn` and `relay` accept mutually exclusive `--here|--window` flags.
  Manual sessions now split the worker into the caller's current window
  (main-vertical by default); spawn-born callers keep the old
  dedicated-window behavior; explicit flags override the auto rule.
- `AGENT_BRIDGE_HERE_LAYOUT` tunes the here placement; values outside the
  tmux layout whitelist abort the spawn before any pane is created.

## Fixes

- Temp files in the task store are now created atomically; a pre-existing
  file is reported as a collision instead of being silently truncated.
- `despawn` verifies the spawn tag in the pane start command, so a recycled
  pane id can no longer be killed by mistake.
MD

# review 情境素材：git repo＋一個含真 bug 的未提交 diff（unquoted 展開）
cat > "$ROOT/project/list-logs.sh" <<'SH'
#!/usr/bin/env bash
# print log files that mention the given pattern
pattern="$1"
for f in logs/*.log; do
  grep -l "$pattern" "$f"
done
SH
git -C "$ROOT/project" init -q
git -C "$ROOT/project" -c user.name=demo -c user.email=demo@example.com \
  add -A
git -C "$ROOT/project" -c user.name=demo -c user.email=demo@example.com \
  commit -qm 'initial import'
cat > "$ROOT/project/list-logs.sh" <<'SH'
#!/usr/bin/env bash
# print log files that mention the given pattern
pattern=$1
for f in logs/*.log; do
  grep -l $pattern $f
done
SH

# scratch 專案的 CLAUDE.md：把「委派」釘到 agent-bridge 流程上
cat > "$ROOT/project/CLAUDE.md" <<'MD'
# Demo project

Delegation in this project goes through the agent-bridge CLI (real worker
panes in tmux). When asked to delegate work to a worker, run:

1. `agent-bridge spawn <name> --runtime codex` — the worker pane opens in
   this window and gets its briefing automatically; wait for the command to
   return (it blocks on the readiness probe).
2. Send the task (multi-line goes through stdin):
   `id=$(agent-bridge send <name> --from main --message-file - <<'EOF' ... EOF)`
3. `agent-bridge await "$id" --timeout 600` then `agent-bridge read "$id"`.
4. Summarize the worker's reply for the user in one or two sentences.

Workers keep their pane context: a follow-up question goes to the SAME
worker with another `send` — never spawn a second worker for a follow-up.

If asked to hand coordination to a successor session: send the task first
(do not await), write a short handoff note to `handoff.md` (task id, current
state, what the successor should do), then run
`agent-bridge relay successor --runtime claude --handoff handoff.md --self-exit main`
and end your turn — the successor session takes over from there.

Keep it lean: no progress files, no extra exploration — delegate right away.
This pane is already registered as `main`.
MD

# 錄影中不能彈權限框：allowlist agent-bridge 與基本讀查
cat > "$ROOT/project/.claude/settings.json" <<'JSON'
{
  "permissions": {
    "allow": [
      "Bash(agent-bridge:*)",
      "Read",
      "Glob",
      "Grep"
    ]
  }
}
JSON

# server 環境（pane 全部繼承）：shim 優先、資料目錄隔離。探針間隔用預設
# 2s——真 codex 啟動期落進輸入框的探針會成為使用者訊息，間隔壓太短只是
# 讓 worker pane 畫面多幾行重複探針。
export PATH="$ROOT/shim:$PATH"
export AGENT_BRIDGE_DATA="$ROOT/data"

# orchestrator pane：先把自己註冊成 main（reply 通知才有落點），再進 claude
left="$(tmux -L "$SOCK" -f /dev/null new-session -dPF '#{pane_id}' \
  -s real -x 200 -y 50 -c "$ROOT/project" \
  'agent-bridge register main "$TMUX_PANE"; exec env -u ANTHROPIC_API_KEY claude')"

# pane 邊框顯示標題，GIF 裡角色醒目（worker 標題由 bridge 自己設）；
# focus-events 開掉 claude 底部的 tmux.conf 提示行（-f /dev/null 沒載使用者設定）
tmux -L "$SOCK" set -g pane-border-status top
tmux -L "$SOCK" set -g focus-events on
tmux -L "$SOCK" select-pane -t "$left" -T orchestrator

echo "real demo env ready: socket=$SOCK root=$ROOT left=$left" >&2
