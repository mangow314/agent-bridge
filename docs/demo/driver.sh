#!/usr/bin/env bash
# README demo 的 worker 側編排器（setup.sh 背景啟動）：
# 等 orchestrator send 的任務送達 stub worker，然後在 worker pane 裡
# 依序敲 start → reply，演出一個會做事的 worker。回覆內容是劇本，
# stub 的 banner 已自報 stand-in 身分。
set -euo pipefail

SOCK=ab-demo
DATA="$1"

tmx() { tmux -L "$SOCK" "$@"; }
# 仿 bin 的通知形狀：文字與 Enter 拆兩次送，避免被 REPL 當貼上吞掉
send() { tmx send-keys -t "$pane" -l "$1"; sleep 0.25; tmx send-keys -t "$pane" Enter; }

# 等第一個任務出現（demo 資料目錄是全新的，出現的必是本次 send）
id=""
for _ in $(seq 1 240); do
  # tasks/ 在首次 agent-bridge 呼叫前不存在；pipefail 下 find 失敗會連坐
  # set -e 殺掉整個 driver，故整條 pipeline 兜底
  id="$(find "$DATA/tasks" -mindepth 1 -maxdepth 1 -printf '%f\n' 2>/dev/null | head -n 1 || true)"
  [[ -n "$id" ]] && break
  sleep 0.5
done
[[ -n "$id" ]] || { echo "driver: 等不到任務" >&2; exit 1; }

# 等 worker pane 的 receive 跑完（狀態翻到 delivered）
for _ in $(seq 1 240); do
  [[ "$(cat "$DATA/tasks/$id/status" 2>/dev/null)" == delivered ]] && break
  sleep 0.5
done

pane="$(jq -r '.pane_id' "$DATA/agents/researcher.json")"

sleep 1.2
send "agent-bridge start $id"
sleep 2.4
send "agent-bridge reply $id --message-file - <<'EOF'"
sleep 0.3
send "Suite green: 1168 PASS / 0 FAIL (bash tests/run-tests.sh)."
sleep 0.3
send "No files modified. Ask follow-ups here — this pane keeps the context."
sleep 0.3
send "EOF"
