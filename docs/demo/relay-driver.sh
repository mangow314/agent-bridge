#!/usr/bin/env bash
# relay 情境編排器（relay.tape 用）：一個 task 跨越交棒仍在跑——
# researcher 開工後先不回覆；successor 註冊後讀交接檔、查 status（running）、
# 這時 researcher 才回覆，successor 收割前任派出的工作，再列 pool 收尾。
set -euo pipefail

DATA="$1"
. "$(dirname "$0")/driver-lib.sh"

# researcher 接工開跑，但把回覆留到交棒之後
id="$(wait_task_n 1)"
wait_status "$id" delivered
pane="$(pane_of researcher)"
sleep 1.5
send "agent-bridge start $id"

# 等接棒 pane 註冊，並留時間讓鏡頭先讀完 orchestrator 側的交棒輸出
# （含盯守提醒）再切窗——切過來正好看到接手者開工
wait_agent successor
pane="$(pane_of successor)"
sleep 6
send "cat /tmp/agent-bridge-demo/handoff.md"
sleep 2.5
send "agent-bridge status $id"
sleep 2

# 前任派的工這時才完工——回覆內容是劇本，stub banner 已自報 stand-in
pane="$(pane_of researcher)"
send "agent-bridge reply $id --message-file - <<'EOF'"
send "p50 2.1ms / p99 8.7ms on the m0.5 suite; no regressions."
send "EOF"
sleep 1.5

# 接手者收割回覆：派工的是前一棒，收的是接棒者
pane="$(pane_of successor)"
send "agent-bridge read $id"
sleep 2.5
send "agent-bridge list"
sleep 2
send "# dispatched by the predecessor, collected by the successor"
