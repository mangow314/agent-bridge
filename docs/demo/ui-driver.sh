#!/usr/bin/env bash
# ui dashboard 情境編排器（ui.tape 用）：兩個 worker 各接一個任務，
# 其中一個開工後停在**權限確認框**——那正是 dashboard 要一眼答出來的狀態。
#
# 框的字面刻意逐字照 agy／claude 的真實形狀（`Do you want to proceed?`
# ＋小寫 `esc to cancel`）：blocker 判定走的是 `notify::screen_has_prompt`
# 同一份 matcher，字面對不上就不是同一件事，GIF 也就演不出真的東西。
set -euo pipefail

DATA="$1"
. "$(dirname "$0")/driver-lib.sh"

# 第一個任務：api-lead 開工、一直跑（GIF 裡看得到 elapsed 在長）
id1="$(wait_task_n 1)"
wait_status "$id1" delivered
pane="$(pane_of api-lead)"
sleep 1
send "agent-bridge start $id1"

# 第二個任務：docs-lead 開工後停在權限框
id2="$(wait_task_n 2)"
wait_status "$id2" delivered
pane="$(pane_of docs-lead)"
sleep 1
send "agent-bridge start $id2"
sleep 1.5
# 印出框本身。用單一 printf 把整框一次寫出去，避免逐行送鍵時被鏡頭拍到
# 半個框；框貼在畫面下緣，正是 matcher 的下緣窗（TAIL_LINES）掃描範圍
send "printf '%s\\n' '' 'Requesting permission for:' '  docs/quickstart.md (write)' '' 'Do you want to proceed?' '> 1. Yes' '  2. Yes, and don'\\''t ask again' '  4. No' '' 'esc to cancel'"
