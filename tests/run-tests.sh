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
assert "events.log 記錄 created" grep -q ' created ' "$D2/tasks/$id2/events.log"

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
  grep -q ' re-receive' "$D3/tasks/$id3/events.log"

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
assert "events.log 記錄 replied 與 read" bash -c \
  "grep -q ' replied' '$D3/tasks/$id3/events.log' && grep -q ' read' '$D3/tasks/$id3/events.log'"

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
  grep -q ' notify-failed ' "$D5/tasks/$id5/events.log"

# ---- 9. tmux 整合：完整 round-trip ----
ab "$DATA_IT" register agent-a "$PANE_A" 2>/dev/null
ab "$DATA_IT" register agent-b "$PANE_B" 2>/dev/null
REQ_IT="$TESTROOT/req-it.md"
printf '整合測試請求 "quoted"\n第二行\n' > "$REQ_IT"
idIT="$(ab "$DATA_IT" send agent-b --from agent-a --message-file "$REQ_IT" 2>/dev/null)"
assert "round-trip：目標 pane 自動執行 receive（→delivered）" \
  wait_for 15 st_is "$DATA_IT" "$idIT" delivered
assert "round-trip：events.log 記錄 notified + delivered" bash -c \
  "grep -q ' notified ' '$DATA_IT/tasks/$idIT/events.log' && grep -q ' delivered' '$DATA_IT/tasks/$idIT/events.log'"
ab "$DATA_IT" reply "$idIT" --message "整合回覆 done" 2>/dev/null
assert "round-trip：reply 後狀態 completed" st_is "$DATA_IT" "$idIT" completed
assert "round-trip：sender pane 收到並執行 read 通知" \
  wait_for 15 grep -q 'Z read$' "$DATA_IT/tasks/$idIT/events.log"

# ---- 總結 ----
printf '\n共 %d PASS、%d FAIL\n' "$PASS" "$FAIL"
if (( FAIL > 0 )); then
  exit 1
fi
exit 0
