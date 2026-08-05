#!/usr/bin/env bash
# 審查情境編排器（review.tape 用）：reviewer worker 接一輪獨立審查任務，
# 回 CONFIRMED／PLAUSIBLE 格式的 findings——演出跨廠牌審查回合的形狀。
# 回覆內容是劇本（stub banner 已自報 stand-in），格式對齊 review 慣例。
set -euo pipefail

DATA="$1"
. "$(dirname "$0")/driver-lib.sh"

id="$(wait_task_n 1)"
wait_status "$id" delivered
pane="$(pane_of reviewer)"

sleep 1.2
send "agent-bridge start $id"
sleep 2.6
send "agent-bridge reply $id --message-file - <<'EOF'"
send "2 findings, 1 confirmed."
send "CONFIRMED tasks/store — metadata.json is written before the bare"
send "status file; reversed order reopens the replayable-transition"
send "window the spec closes. Reorder: status first, metadata second."
send "PLAUSIBLE notify/guard — dialog match is version-pinned; fails"
send "open on UI copy changes. Known limit, flagging for the record."
send "EOF"
