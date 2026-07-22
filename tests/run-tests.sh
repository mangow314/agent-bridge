#!/usr/bin/env bash
# agent-bridge 測試（純 bash，零額外依賴）
# - 一律用 AGENT_BRIDGE_DATA 指向暫存目錄，不碰真實資料
# - tmux 整合測試只用獨立 socket：tmux -L agent-bridge-test -f /dev/null
set -u
unset TMUX

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BRIDGE="$ROOT/bin/agent-bridge"
SOCK="agent-bridge-test"

if ! REAL_TMUX="$(command -v tmux)"; then
  echo "跑測試需要 tmux" >&2
  exit 1
fi

TESTROOT="$(mktemp -d "${TMPDIR:-/tmp}/agent-bridge-tests.XXXXXX")"

# await 測試走短輪詢間隔，避免整體測試時間被預設 1s 拖長
export AGENT_BRIDGE_POLL_INTERVAL=0.2

tmx() { "$REAL_TMUX" -L "$SOCK" -f /dev/null "$@"; }

# shellcheck disable=SC2329  # 經 trap EXIT 間接呼叫
cleanup() {
  tmx kill-server 2>/dev/null || true
  if [[ -n "${TESTROOT:-}" && -d "$TESTROOT" ]]; then
    rm -rf -- "$TESTROOT"
  fi
}
trap cleanup EXIT

PASS=0
FAIL=0
ok()  { PASS=$((PASS + 1)); printf 'PASS: %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf 'FAIL: %s\n' "$1"; }

# assert <desc> <cmd...>：cmd 成功則 PASS
assert() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then ok "$desc"; else bad "$desc"; fi
}
# assert_fails <desc> <cmd...>：cmd 失敗（非零退出）則 PASS
assert_fails() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then bad "$desc"; else ok "$desc"; fi
}

# wait_for <timeout-secs> <cmd...>：輪詢直到 cmd 成功或逾時
wait_for() {
  local timeout="$1"; shift
  local i
  for (( i = 0; i < timeout * 5; i++ )); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  "$@" >/dev/null 2>&1
}

# evt_grep <events.log> <event-name>：以「時間戳（...Z）後緊接事件名、
# 事件名後接空白或行尾」為界比對，避免子字串誤判
# （例：'failed' 誤中 'notify-failed' 的 'y-failed'）
# shellcheck disable=SC2329  # 經 assert/wait_for 的 "$@" 間接呼叫
evt_grep() {
  local log="$1" ev="$2"
  grep -qE "Z ${ev}([[:space:]]|\$)" "$log"
}

# ---- shim：讓 bridge 內部的 tmux 呼叫走測試 socket ----
SHIM="$TESTROOT/shim"
FAILSHIM="$TESTROOT/failshim"
mkdir -p "$SHIM" "$FAILSHIM"
printf '#!/usr/bin/env bash\nunset TMUX\nexec %q -L %q -f /dev/null "$@"\n' \
  "$REAL_TMUX" "$SOCK" > "$SHIM/tmux"
chmod +x "$SHIM/tmux"
ln -s "$BRIDGE" "$SHIM/agent-bridge"
# failshim：模擬 tmux 不可用
printf '#!/usr/bin/env bash\necho "tmux: unavailable (test stub)" >&2\nexit 1\n' \
  > "$FAILSHIM/tmux"
chmod +x "$FAILSHIM/tmux"

# ab <data-dir> <args...>：以指定資料目錄 + shim PATH 執行 bridge
ab() {
  local data="$1"; shift
  env AGENT_BRIDGE_DATA="$data" PATH="$SHIM:$PATH" "$BRIDGE" "$@"
}
# ab_notmux：tmux 不可用情境
# shellcheck disable=SC2329  # 經 assert/assert_fails 的 "$@" 間接呼叫
ab_notmux() {
  local data="$1"; shift
  env AGENT_BRIDGE_DATA="$data" PATH="$FAILSHIM:$PATH" "$BRIDGE" "$@"
}
# shellcheck disable=SC2329  # 經 assert/wait_for 的 "$@" 間接呼叫
st_is() { [[ "$(ab "$1" status "$2" 2>/dev/null)" == "$3" ]]; }

# ---- 測試 tmux server：兩個假 pane（跑 bash） ----
DATA_IT="$TESTROOT/data-it"
pane_cmd="$(printf 'env AGENT_BRIDGE_DATA=%q PATH=%q bash --norc --noprofile' \
  "$DATA_IT" "$SHIM:$PATH")"
tmx new-session -d -s it -x 180 -y 40 "$pane_cmd"
tmx split-window -d -t it "$pane_cmd"
mapfile -t PANES < <(tmx list-panes -t it -F '#{pane_id}')
PANE_A="${PANES[0]}"
PANE_B="${PANES[1]}"

# 等兩個假 pane 的 bash 就緒（能執行 send-keys 指令）
tmx send-keys -t "$PANE_A" "$(printf 'touch %q' "$TESTROOT/ready-a")" Enter
tmx send-keys -t "$PANE_B" "$(printf 'touch %q' "$TESTROOT/ready-b")" Enter
if ! wait_for 10 test -f "$TESTROOT/ready-a" || ! wait_for 10 test -f "$TESTROOT/ready-b"; then
  echo "測試 pane 未就緒，中止" >&2
  exit 1
fi

# ---- 1. register / list（含同名覆蓋） ----
D1="$TESTROOT/d1"
ab "$D1" register alice "$PANE_A" 2>/dev/null
assert "register+list：alice 對應 pane id" \
  test "$(ab "$D1" list)" = "$(printf 'alice\t%s' "$PANE_A")"
ab "$D1" register alice "$PANE_B" 2>/dev/null
assert "同名 register 覆蓋 pane id" \
  test "$(ab "$D1" list)" = "$(printf 'alice\t%s' "$PANE_B")"
assert "覆蓋後 list 仍只有一行" \
  test "$(ab "$D1" list | wc -l)" -eq 1

assert_fails "register：不合法 agent 名（含空白）被拒" \
  ab "$D1" register "bad name" "$PANE_A"
assert_fails "register：無法解析的 tmux target 報錯" \
  ab "$D1" register ghosty "no-such-session:99.9"
assert "register 失敗時不寫 agents 檔" \
  test ! -e "$D1/agents/ghosty.json"
assert_fails "register：tmux 不可用時報錯" \
  ab_notmux "$D1" register carol "$PANE_A"

# ---- 2. send 錯誤路徑 ----
out="$(ab "$D1" send nobody --from alice --message hi 2>/dev/null)"; rc=$?
assert "send 未註冊 agent：非零退出" test "$rc" -ne 0
assert "send 未註冊 agent：stdout 為空" test -z "$out"
assert_fails "send 缺 --from 報錯" ab "$D1" send alice --message hi
assert_fails "send 缺訊息參數報錯" ab "$D1" send alice --from bob
assert_fails "send：--from 名稱不合法被拒" \
  ab "$D1" send alice --from "bad name" --message hi

# ---- 3. 未知 task 的 receive / status / read ----
assert_fails "receive 未知 task 報錯" ab "$D1" receive 20990101T000000Z-dead
assert_fails "status 未知 task 報錯"  ab "$D1" status  20990101T000000Z-dead
assert_fails "read 未知 task 報錯"    ab "$D1" read    20990101T000000Z-dead

# ---- 4. send 快樂路徑 + read 於未 completed ----
# 註：pane 的 AGENT_BRIDGE_DATA 指向 DATA_IT，收到本組（D2）通知後其 receive 會
# 找不到 task 而失敗，不影響 D2 的狀態——正好隔離出 queued 狀態供測試。
D2="$TESTROOT/d2"
ab "$D2" register bob "$PANE_B" 2>/dev/null
id2="$(ab "$D2" send bob --from alice --message "hello bob" 2>"$TESTROOT/d2-send.err")"
rc=$?
assert "send 成功：exit 0" test "$rc" -eq 0
assert "send stdout 只有一行 task-id" \
  test "$(printf '%s\n' "$id2" | wc -l)" -eq 1
assert "task-id 格式：UTC 時間戳前綴＋短隨機後綴" \
  bash -c "[[ '$id2' =~ ^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{4}$ ]]"
assert "send 後狀態為 queued" st_is "$D2" "$id2" queued
assert "task 目錄含 metadata.json/request.md/status/events.log" bash -c \
  "test -f '$D2/tasks/$id2/metadata.json' && test -f '$D2/tasks/$id2/request.md' \
   && test -f '$D2/tasks/$id2/status' && test -f '$D2/tasks/$id2/events.log'"
assert "metadata version 為 JSON number 1" \
  bash -c "[[ \"\$(jq '.version' '$D2/tasks/$id2/metadata.json')\" == 1 ]]"
assert "events.log 記錄 created" evt_grep "$D2/tasks/$id2/events.log" created

ab "$D2" read "$id2" >/dev/null 2>"$TESTROOT/d2-read.err"; rc=$?
assert "read 於尚未 completed：非零退出" test "$rc" -ne 0
assert "read 於尚未 completed：stderr 提示尚未回覆" \
  grep -q '尚未回覆' "$TESTROOT/d2-read.err"

# ---- 5. reply 非法轉換：queued 未 receive 直接 reply ----
ab "$D2" reply "$id2" --message "premature" >/dev/null 2>&1; rc=$?
assert "reply 於 queued（未 receive）：非零退出" test "$rc" -ne 0
assert "reply 於 queued：狀態檔不變（仍 queued）" st_is "$D2" "$id2" queued
assert "reply 於 queued：不產生 response.md" \
  test ! -e "$D2/tasks/$id2/response.md"

# ---- 6. 特殊字元 byte-for-byte 保真（--message-file 與 stdin） ----
D3="$TESTROOT/d3"
ab "$D3" register bob "$PANE_B" 2>/dev/null
TRICKY="$TESTROOT/tricky.txt"
# shellcheck disable=SC2016  # 刻意保留字面 $VAR/`cmd`，測試保真
printf 'line1 "double" '\''single'\''\n\ttab\n$VAR `cmd` \\backslash\n中文 emoji 🚀\nno-trailing-newline' > "$TRICKY"

id3="$(ab "$D3" send bob --from alice --message-file "$TRICKY" 2>/dev/null)"
ab "$D3" receive "$id3" > "$TESTROOT/got3.out" 2> "$TESTROOT/got3.hdr"
assert "receive stdout 與原文 diff 為空（特殊字元保真）" \
  diff -q "$TRICKY" "$TESTROOT/got3.out"
assert "receive 後狀態為 delivered" st_is "$D3" "$id3" delivered
assert "receive stderr 標頭含 task-id" grep -q "task-id: $id3" "$TESTROOT/got3.hdr"
assert "receive stderr 標頭含 from" grep -q 'from: alice' "$TESTROOT/got3.hdr"
assert "receive stderr 標頭含 working_directory" \
  grep -q 'working_directory: ' "$TESTROOT/got3.hdr"

ab "$D3" receive "$id3" > "$TESTROOT/got3b.out" 2>/dev/null
assert "re-receive 冪等：內容重印一致" diff -q "$TRICKY" "$TESTROOT/got3b.out"
assert "re-receive 冪等：狀態仍 delivered" st_is "$D3" "$id3" delivered
assert "re-receive 記入 events.log" \
  evt_grep "$D3/tasks/$id3/events.log" re-receive

id3s="$(ab "$D3" send bob --from alice --message-file - < "$TRICKY" 2>/dev/null)"
ab "$D3" receive "$id3s" > "$TESTROOT/got3s.out" 2>/dev/null
assert "send --message-file - （stdin）保真" diff -q "$TRICKY" "$TESTROOT/got3s.out"

# ---- 7. reply / read / 對 completed 重複 reply ----
TRICKY2="$TESTROOT/tricky2.txt"
# shellcheck disable=SC2016  # 刻意保留字面 $x/`y`，測試保真
printf 'reply "with" quotes\n多行回覆 $x `y`\nend' > "$TRICKY2"
ab "$D3" reply "$id3" --message-file "$TRICKY2" 2>/dev/null
assert "reply 後狀態為 completed" st_is "$D3" "$id3" completed
ab "$D3" read "$id3" > "$TESTROOT/rgot3.out" 2> "$TESTROOT/rgot3.hdr"
assert "read stdout 與 response 原文 diff 為空" \
  diff -q "$TRICKY2" "$TESTROOT/rgot3.out"
assert "read stderr 標頭含 task-id" grep -q "task-id: $id3" "$TESTROOT/rgot3.hdr"
assert "events.log 記錄 replied" evt_grep "$D3/tasks/$id3/events.log" replied
assert "events.log 記錄 read" evt_grep "$D3/tasks/$id3/events.log" read

cp "$D3/tasks/$id3/status" "$TESTROOT/status-before"
ab "$D3" reply "$id3" --message "again" >/dev/null 2>&1; rc=$?
assert "對 completed 重複 reply：非零退出" test "$rc" -ne 0
assert "對 completed 重複 reply：狀態檔內容不變" \
  diff -q "$TESTROOT/status-before" "$D3/tasks/$id3/status"

# ---- 8. 通知失敗路徑（pane 已死） ----
D5="$TESTROOT/d5"
p3="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D5" register carol "$p3" 2>/dev/null
tmx kill-pane -t "$p3"
id5="$(ab "$D5" send carol --from alice --message hi 2>"$TESTROOT/d5-send.err")"
rc=$?
assert "通知失敗：send 仍 exit 0" test "$rc" -eq 0
assert "通知失敗：task 照常建立（queued）" st_is "$D5" "$id5" queued
assert "通知失敗：stderr 有含 task-id 的警告" \
  grep -q "$id5" "$TESTROOT/d5-send.err"
assert "通知失敗：events.log 記 notify-failed" \
  evt_grep "$D5/tasks/$id5/events.log" notify-failed

# ---- 8b. 鎖失敗路徑：權限失敗 vs 真正鎖佔用 ----
D6="$TESTROOT/d6"
ab "$D6" register bob "$PANE_B" 2>/dev/null
id6="$(ab "$D6" send bob --from alice --message hi 2>/dev/null)"
chmod 555 "$D6/locks"
ab "$D6" receive "$id6" >/dev/null 2>"$TESTROOT/d6-recv.err"; rc=$?
chmod 755 "$D6/locks"
assert "鎖目錄不可寫：非零退出" test "$rc" -ne 0
assert "鎖目錄不可寫：報權限問題而非鎖佔用" \
  grep -q '非鎖佔用' "$TESTROOT/d6-recv.err"
assert "鎖目錄不可寫：狀態不變（仍 queued）" st_is "$D6" "$id6" queued
mkdir "$D6/locks/$id6.lock"
ab "$D6" receive "$id6" >/dev/null 2>"$TESTROOT/d6-lock.err"; rc=$?
rmdir "$D6/locks/$id6.lock"
assert "鎖被佔用：重試後非零退出" test "$rc" -ne 0
assert "鎖被佔用：報佔用中" grep -q '佔用中' "$TESTROOT/d6-lock.err"

# ---- 9. unregister ----
D7="$TESTROOT/d7"
ab "$D7" register dave "$PANE_A" 2>/dev/null
ab "$D7" unregister dave 2>/dev/null; rc=$?
assert "unregister：exit 0" test "$rc" -eq 0
assert "unregister 後 list 為空" test -z "$(ab "$D7" list)"
assert "unregister 後 agents 檔已刪" test ! -e "$D7/agents/dave.json"
assert_fails "unregister 未註冊 agent 報錯" ab "$D7" unregister nobody
assert_fails "unregister：不合法名稱被拒" ab "$D7" unregister "bad name"
assert_fails "unregister 後 send 該 agent 報錯" \
  ab "$D7" send dave --from alice --message hi

# ---- 10. start：delivered → running ----
D8="$TESTROOT/d8"
ab "$D8" register bob "$PANE_B" 2>/dev/null
id8="$(ab "$D8" send bob --from alice --message "work" 2>/dev/null)"
assert_fails "start 於 queued（未 receive）報錯" ab "$D8" start "$id8"
assert "start 於 queued：狀態不變" st_is "$D8" "$id8" queued
ab "$D8" receive "$id8" >/dev/null 2>&1
ab "$D8" start "$id8" 2>/dev/null; rc=$?
assert "start 於 delivered：exit 0" test "$rc" -eq 0
assert "start 後狀態 running" st_is "$D8" "$id8" running
assert "events.log 記 started" evt_grep "$D8/tasks/$id8/events.log" started
assert_fails "重複 start（running）報錯" ab "$D8" start "$id8"
ab "$D8" receive "$id8" > "$TESTROOT/got8.out" 2>/dev/null
assert "receive 於 running 冪等：內容一致" \
  bash -c "diff <(printf 'work\n') '$TESTROOT/got8.out'"
assert "receive 於 running：狀態仍 running" st_is "$D8" "$id8" running
ab "$D8" reply "$id8" --message "done from running" 2>/dev/null
assert "running 可 reply → completed" st_is "$D8" "$id8" completed

# ---- 11. fail：delivered/running → failed，read 可讀失敗原因 ----
D9="$TESTROOT/d9"
ab "$D9" register bob "$PANE_B" 2>/dev/null
id9="$(ab "$D9" send bob --from alice --message "doomed" 2>/dev/null)"
assert_fails "fail 於 queued 報錯" ab "$D9" fail "$id9" --message "nope"
assert "fail 於 queued：不產生 response.md" test ! -e "$D9/tasks/$id9/response.md"
ab "$D9" receive "$id9" >/dev/null 2>&1
assert_fails "fail 缺訊息參數報錯" ab "$D9" fail "$id9"
FAILMSG="$TESTROOT/failmsg.txt"
printf '失敗原因："權限不足"\n第二行\n' > "$FAILMSG"
ab "$D9" fail "$id9" --message-file "$FAILMSG" 2>/dev/null; rc=$?
assert "fail 於 delivered：exit 0" test "$rc" -eq 0
assert "fail 後狀態 failed" st_is "$D9" "$id9" failed
assert "events.log 記 failed" evt_grep "$D9/tasks/$id9/events.log" failed
ab "$D9" read "$id9" > "$TESTROOT/rgot9.out" 2> "$TESTROOT/rgot9.hdr"; rc=$?
assert "read 於 failed：exit 0" test "$rc" -eq 0
assert "read 於 failed：內容與失敗原因 diff 為空" diff -q "$FAILMSG" "$TESTROOT/rgot9.out"
assert "read 於 failed：stderr 標頭含 task-id" grep -q "task-id: $id9" "$TESTROOT/rgot9.hdr"
assert_fails "fail 後 reply 報錯（終態）" ab "$D9" reply "$id9" --message late
assert_fails "重複 fail 報錯（終態）" ab "$D9" fail "$id9" --message again
assert "終態後狀態仍 failed" st_is "$D9" "$id9" failed

# ---- 12. cancel：queued/delivered/running → cancelled ----
D10="$TESTROOT/d10"
ab "$D10" register bob "$PANE_B" 2>/dev/null
idc1="$(ab "$D10" send bob --from alice --message "c1" 2>/dev/null)"
ab "$D10" cancel "$idc1" 2>/dev/null; rc=$?
assert "cancel 於 queued：exit 0" test "$rc" -eq 0
assert "cancel 後狀態 cancelled" st_is "$D10" "$idc1" cancelled
assert "events.log 記 cancelled" evt_grep "$D10/tasks/$idc1/events.log" cancelled
assert "cancel 通知 worker（events 記 cmd=status）" \
  grep -q 'cmd=status$' "$D10/tasks/$idc1/events.log"
assert_fails "cancelled 後 receive 報錯" ab "$D10" receive "$idc1"
assert_fails "cancelled 後 reply 報錯" ab "$D10" reply "$idc1" --message x
ab "$D10" read "$idc1" >/dev/null 2>"$TESTROOT/rc1.err"; rc=$?
assert "cancelled 後 read 非零退出" test "$rc" -ne 0
assert "cancelled 後 read 提示已取消" grep -q '已取消' "$TESTROOT/rc1.err"
assert_fails "重複 cancel 報錯" ab "$D10" cancel "$idc1"
idc2="$(ab "$D10" send bob --from alice --message "c2" 2>/dev/null)"
ab "$D10" receive "$idc2" >/dev/null 2>&1
ab "$D10" start "$idc2" 2>/dev/null
ab "$D10" cancel "$idc2" 2>/dev/null
assert "cancel 於 running → cancelled" st_is "$D10" "$idc2" cancelled
assert_fails "cancelled 後 start 報錯" ab "$D10" start "$idc2"
idc3="$(ab "$D10" send bob --from alice --message "c3" 2>/dev/null)"
ab "$D10" receive "$idc3" >/dev/null 2>&1
ab "$D10" reply "$idc3" --message ok 2>/dev/null
assert_fails "cancel 於 completed 報錯" ab "$D10" cancel "$idc3"
assert "cancel 失敗後狀態仍 completed" st_is "$D10" "$idc3" completed

# delivered → cancelled（不經 start，直接對已 receive 的 task 取消）
idc4="$(ab "$D10" send bob --from alice --message "c4" 2>/dev/null)"
ab "$D10" receive "$idc4" >/dev/null 2>&1
assert "cancel 於 delivered：cancel 前狀態為 delivered" st_is "$D10" "$idc4" delivered
ab "$D10" cancel "$idc4" 2>/dev/null; rc=$?
assert "cancel 於 delivered：exit 0" test "$rc" -eq 0
assert "cancel 於 delivered → cancelled" st_is "$D10" "$idc4" cancelled
assert "cancel 於 delivered：events.log 記 cancelled" \
  evt_grep "$D10/tasks/$idc4/events.log" cancelled
assert_fails "cancel 於 delivered 後 reply 報錯" \
  ab "$D10" reply "$idc4" --message late

# ---- 13. await：等待終態 ----
assert "await 於 completed：立即印 completed" \
  test "$(ab "$D3" await "$id3" 2>/dev/null)" = completed
assert "await 於 failed：印 failed" \
  test "$(ab "$D9" await "$id9" 2>/dev/null)" = failed
assert "await 於 cancelled：印 cancelled" \
  test "$(ab "$D10" await "$idc1" 2>/dev/null)" = cancelled
assert_fails "await 未知 task 報錯" ab "$D2" await 20990101T000000Z-dead
assert_fails "await：--timeout 非整數被拒" ab "$D2" await "$id2" --timeout abc
ab "$D2" await "$id2" --timeout 1 >/dev/null 2>"$TESTROOT/await-to.err"; rc=$?
assert "await 逾時：非零退出" test "$rc" -ne 0
assert "await 逾時：訊息含目前狀態" grep -q 'queued' "$TESTROOT/await-to.err"
# 回歸（codex 複核 finding）：前導零 timeout 曾被 Bash 算術當八進位，
# 08 令逾時永不觸發且輪詢每圈爆 "value too great for base"
assert "await：--timeout 前導零（08）通過驗證" \
  test "$(ab "$D3" await "$id3" --timeout 08 2>/dev/null)" = completed
timeout 1 env AGENT_BRIDGE_DATA="$D2" AGENT_BRIDGE_POLL_INTERVAL=0.05 \
  "$BRIDGE" await "$id2" --timeout 08 >/dev/null 2>"$TESTROOT/await-oct.err"
assert "await：--timeout 08 輪詢無八進位算術錯誤" \
  bash -c "! grep -q 'value too great' '$TESTROOT/await-oct.err'"
assert_fails "await：--timeout 超過 9 位被拒" ab "$D2" await "$id2" --timeout 1234567890
D11="$TESTROOT/d11"
ab "$D11" register bob "$PANE_B" 2>/dev/null
id11="$(ab "$D11" send bob --from alice --message "bg" 2>/dev/null)"
ab "$D11" receive "$id11" >/dev/null 2>&1
ab "$D11" await "$id11" --timeout 15 > "$TESTROOT/await-bg.out" 2>/dev/null &
AWAIT_PID=$!
sleep 0.5
ab "$D11" reply "$id11" --message bg-done 2>/dev/null
wait "$AWAIT_PID"; rc=$?
assert "背景 await：reply 後返回 exit 0" test "$rc" -eq 0
assert "背景 await：印出 completed" \
  test "$(cat "$TESTROOT/await-bg.out")" = completed

# ---- 14. tmux 整合：完整 round-trip ----
ab "$DATA_IT" register agent-a "$PANE_A" 2>/dev/null
ab "$DATA_IT" register agent-b "$PANE_B" 2>/dev/null
REQ_IT="$TESTROOT/req-it.md"
printf '整合測試請求 "quoted"\n第二行\n' > "$REQ_IT"
idIT="$(ab "$DATA_IT" send agent-b --from agent-a --message-file "$REQ_IT" 2>/dev/null)"
assert "round-trip：目標 pane 自動執行 receive（→delivered）" \
  wait_for 15 st_is "$DATA_IT" "$idIT" delivered
assert "round-trip：events.log 記錄 notified" \
  evt_grep "$DATA_IT/tasks/$idIT/events.log" notified
assert "round-trip：events.log 記錄 delivered" \
  evt_grep "$DATA_IT/tasks/$idIT/events.log" delivered
ab "$DATA_IT" reply "$idIT" --message "整合回覆 done" 2>/dev/null
assert "round-trip：reply 後狀態 completed" st_is "$DATA_IT" "$idIT" completed
assert "round-trip：sender pane 收到並執行 read 通知" \
  wait_for 15 grep -q 'Z read$' "$DATA_IT/tasks/$idIT/events.log"

# ---- 15. 併發壓測：衝突轉換 + 大量平行 send ----

# 15a. 同一 task 於 delivered 狀態並行射出 3 reply + 3 fail + 3 cancel：
# 鎖序列化下應恰有一者得手，其餘全被狀態檢查擋下；驗證無雙終態、無殘鎖
D12="$TESTROOT/d12"
ab "$D12" register bob "$PANE_B" 2>/dev/null
idp="$(ab "$D12" send bob --from alice --message "race" 2>/dev/null)"
ab "$D12" receive "$idp" >/dev/null 2>&1
for i in 1 2 3; do
  ab "$D12" reply  "$idp" --message "r$i" >/dev/null 2>&1 &
  ab "$D12" fail   "$idp" --message "f$i" >/dev/null 2>&1 &
  ab "$D12" cancel "$idp"                 >/dev/null 2>&1 &
done
wait

# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
is_terminal_status() {
  case "$1" in
    completed|failed|cancelled) return 0 ;;
    *) return 1 ;;
  esac
}
final_st="$(ab "$D12" status "$idp" 2>/dev/null)"
assert "併發衝突轉換：最終狀態恰為終態三者之一" is_terminal_status "$final_st"

term_count() {
  grep -cE 'Z (replied|failed|cancelled)([[:space:]]|$)' "$1"
}
assert "併發衝突轉換：events.log 恰有一個終態事件" \
  test "$(term_count "$D12/tasks/$idp/events.log")" -eq 1
assert "併發衝突轉換：locks 目錄無殘鎖" \
  test -z "$(ls -A "$D12/locks" 2>/dev/null)"

# 15b. 同一 sender/receiver 並行 10 個 send：驗證 10 個互異 task-id、
# 各自目錄存在且狀態合法
D13="$TESTROOT/d13"
ab "$D13" register bob "$PANE_B" 2>/dev/null
PARDIR="$TESTROOT/par-send"
mkdir -p "$PARDIR"
for i in $(seq 1 10); do
  ab "$D13" send bob --from alice --message "concurrent $i" \
    > "$PARDIR/$i.id" 2>/dev/null &
done
wait
mapfile -t ids13 < <(cat "$PARDIR"/*.id)
assert "併發 send：產生 10 個 task-id" test "${#ids13[@]}" -eq 10
assert "併發 send：10 個 task-id 互異" \
  test "$(printf '%s\n' "${ids13[@]}" | sort -u | wc -l)" -eq 10

# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
all_ids13_ok() {
  local id st
  for id in "${ids13[@]}"; do
    [[ -d "$D13/tasks/$id" ]] || return 1
    st="$(ab "$D13" status "$id" 2>/dev/null)" || return 1
    case "$st" in
      queued|delivered|running|completed|failed|cancelled) ;;
      *) return 1 ;;
    esac
  done
  return 0
}
assert "併發 send：10 個 task 目錄皆存在且狀態合法" all_ids13_ok

# ---- 總結 ----
printf '\n共 %d PASS、%d FAIL\n' "$PASS" "$FAIL"
if (( FAIL > 0 )); then
  exit 1
fi
exit 0
