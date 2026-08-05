#!/usr/bin/env bash
# 討論情境編排器（discussion.tape 用）：同一個 advisor worker 連接兩輪任務，
# 第二輪回覆明確引用第一輪的立場——演出「pane 保留脈絡，追問不必重講前情」。
set -euo pipefail

DATA="$1"
. "$(dirname "$0")/driver-lib.sh"

id1="$(wait_task_n 1)"
wait_status "$id1" delivered
pane="$(pane_of advisor)"

sleep 1.2
send "agent-bridge start $id1"
sleep 2.2
send "agent-bridge reply $id1 --message-file - <<'EOF'"
send "Position: keep the timestamp prefix — audit logs and gc ordering"
send "come for free. Risk: two hosts minting ids in the same second can"
send "collide; the random suffix only narrows that, not closes it."
send "EOF"

id2="$(wait_task_n 2)"
wait_status "$id2" delivered

sleep 1.2
send "agent-bridge start $id2"
sleep 2.2
send "agent-bridge reply $id2 --message-file - <<'EOF'"
send "Single-host minting removes the risk I raised in round 1: one"
send "clock serializes ids, and the suffix already covers same-second"
send "bursts. Position unchanged — keep the timestamp prefix."
send "EOF"
