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
  "$REAL_TMUX" -L agent-bridge-test-b -f /dev/null kill-server 2>/dev/null || true
  "$REAL_TMUX" -L agent-bridge-test-c -f /dev/null kill-server 2>/dev/null || true
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

# ---- 假 runtime：spawn 測試用的慢啟動 REPL ----
# 啟動期（預設 2 秒，<rt>-delay 檔可調）持續讀掉並丟棄 stdin，模擬
# 「REPL 啟動期按鍵被吃」；之後 exec bash 執行後續抵達的探針／通知。
# <rt>-fail 檔存在時立即以非零碼退出，模擬 runtime 啟動即失敗。
# 每個 runtime 各自一份 args/argv/delay/fail 檔，互不干擾。
# 兩種 args 格式並存是刻意的：`-args.txt` 一行式（空白分隔）方便比對相鄰的
# 旗標＋值，例如 `--profile agent-worker`；`-argv.txt` 以 NUL 分隔，保留 argv
# 邊界，是唯一能斷言「完整參數集合恰好是什麼」的形式——子字串比對擋不住
# 「在後面偷偷追加一個旗標」這類退化（獨立複核以 mutation 反例證明）。
# 分隔符必須是 NUL 不能是換行：worker brief prompt 本身含大量換行，
# 用換行分隔會讓「參數個數」變成「brief 行數」，斷言直接失去意義。
DSPAWN="$TESTROOT/dspawn"
make_runtime_shim() {
  local rt="$1"
  cat > "$SHIM/$rt" <<EOF
#!/usr/bin/env bash
printf '%s ' "\$@" > "$TESTROOT/$rt-args.txt"
printf '%s\0' "\$@" > "$TESTROOT/$rt-argv.txt"
if [[ -e "$TESTROOT/$rt-fail" ]]; then exit 127; fi
export AGENT_BRIDGE_DATA="$DSPAWN"
delay="\$(cat "$TESTROOT/$rt-delay" 2>/dev/null || echo 2)"
end=\$(( SECONDS + delay ))
while (( SECONDS < end )); do IFS= read -r -t 0.2 _ || true; done
exec bash --norc --noprofile
EOF
  chmod +x "$SHIM/$rt"
}
make_runtime_shim codex
# 真 claude 也在 PATH 上，但 shim 排在最前面，測試不會叫到真的
make_runtime_shim claude

# spawn 出來的 pane 由 tmux server 啟動，PATH 繼承自 server 環境；
# 在 server 啟動前把 shim 排進 PATH，pane 才找得到假 codex 與 agent-bridge
export PATH="$SHIM:$PATH"

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

# ---- spawn 測試 helper ----
# absp <data-dir> <ready-timeout> <args...>：spawn 用短探針間隔執行 bridge
absp() {
  local data="$1" rt="$2"; shift 2
  env AGENT_BRIDGE_DATA="$data" AGENT_BRIDGE_READY_TIMEOUT="$rt" \
      AGENT_BRIDGE_READY_PROBE_INTERVAL=0.5 PATH="$SHIM:$PATH" "$BRIDGE" "$@"
}
# shellcheck disable=SC2329  # 經 assert/assert_fails 的 "$@" 間接呼叫
pane_alive() { tmx list-panes -a -F '#{pane_id}' 2>/dev/null | grep -Fx "$1" >/dev/null; }
pane_count() { tmx list-panes -a -F '#{pane_id}' 2>/dev/null | wc -l; }
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
list_has() { ab "$1" list 2>/dev/null | grep -Fqx "$2"; }

# ---- 測試 tmux server：兩個假 pane（跑 bash） ----
DATA_IT="$TESTROOT/data-it"
pane_cmd="$(printf 'env AGENT_BRIDGE_DATA=%q PATH=%q bash --norc --noprofile' \
  "$DATA_IT" "$SHIM:$PATH")"
tmx new-session -d -s it -x 200 -y 100 "$pane_cmd"
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
assert "register+list：alice 對應 pane id（人工註冊 ready 欄為 -）" \
  test "$(ab "$D1" list)" = "$(printf 'alice\t%s\t-' "$PANE_A")"
ab "$D1" register alice "$PANE_B" 2>/dev/null
assert "同名 register 覆蓋 pane id" \
  test "$(ab "$D1" list)" = "$(printf 'alice\t%s\t-' "$PANE_B")"
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

# ---- 8a. 盲點 1：worker 停在權限對話框時，通知的按鍵不得送進去 ----
# notify_pane 送鍵前 capture-pane 掃 Claude Code 權限對話框特徵，掃到就降級。這裡
# 直接鎖住核心不變量「攔截時一個按鍵都沒送進 pane」——而非只驗 return code：假 pane
# 印出對話框特徵後進 `while read` 迴圈、把收到的每一行記進 got 檔。攔截生效＝got 不
# 含通知文字。為排除「其實送了、只是還沒 append」的偽陰性，先送一個哨兵行、確認 got
# 記得到它（證明記錄機制活著），再斷言 got 裡沒有 task-id。
# 註：capture 失敗 fail-closed（B1）與送 Enter 前二次掃描（B3）是邏輯路徑，真 tmux
# 環境難穩定構造「capture 失敗但 send-keys 成功」與「文字送出後才彈框」，由 code
# review＋shellcheck 把關，不在此列測例。
D5a="$TESTROOT/d5a"
p_ask="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D5a" register dodo "$p_ask" 2>/dev/null
tmx send-keys -t "$p_ask" \
  "printf '%s\n' 'Do you want to proceed?' 'Esc to cancel · Tab to amend' ; touch $TESTROOT/ask1-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/ask1-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/ask1-ready"
id5a="$(ab "$D5a" send dodo --from alice --message hi 2>"$TESTROOT/d5a-send.err")"
rc=$?
tmx send-keys -t "$p_ask" 'SENTINEL-ask1' Enter
assert "對話框偵測(Bash 框)：send 仍 exit 0" test "$rc" -eq 0
assert "對話框偵測(Bash 框)：記錄機制活著（哨兵行有進 got）" \
  wait_for 10 grep -q 'SENTINEL-ask1' "$TESTROOT/ask1-got.txt"
# 恰好只有哨兵一行才算攔截成功：只 grep task-id 缺席擋不住「只送一個 Enter 再
# return 1」的 regression——那會讓 got 多一個空白行、仍不含 task-id 卻已送出危險
# 的 Enter。改斷言 got 內容整行等於哨兵、且列數恰為 1，空白行／額外 Enter／任何
# 通知文字都會讓它失敗（二輪複核 B4）
assert "對話框偵測(Bash 框)：got 恰好只有哨兵一行（通知的文字與 Enter 都沒送進來）" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -Fxq "$2" "$1"' _ "$TESTROOT/ask1-got.txt" 'SENTINEL-ask1'
assert "對話框偵測(Bash 框)：events.log 記 notify-failed" \
  evt_grep "$D5a/tasks/$id5a/events.log" notify-failed
assert "對話框偵測(Bash 框)：stderr 有含 task-id 的警告" \
  grep -q "$id5a" "$TESTROOT/d5a-send.err"
tmx kill-pane -t "$p_ask" 2>/dev/null || true

# 8a②：Edit/Write 型的權限框標題不是「Do you want to proceed?」而是「Do you want to
# make this edit to …」。特徵用「Do you want to 」前綴正是為了涵蓋這些型別；只認
# proceed 會漏判、退回會誤觸的行為（B2）
D5c="$TESTROOT/d5c"
p_edit="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D5c" register didi "$p_edit" 2>/dev/null
tmx send-keys -t "$p_edit" \
  "printf '%s\n' 'Do you want to make this edit to foo.rs?' 'Esc to cancel · Tab to amend' ; touch $TESTROOT/ask2-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/ask2-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/ask2-ready"
id5c="$(ab "$D5c" send didi --from alice --message hi 2>/dev/null)"
tmx send-keys -t "$p_edit" 'SENTINEL-ask2' Enter
assert "對話框偵測(Edit 框)：記錄機制活著（哨兵行有進 got）" \
  wait_for 10 grep -q 'SENTINEL-ask2' "$TESTROOT/ask2-got.txt"
assert "對話框偵測(Edit 框)：got 恰好只有哨兵一行（Edit 型也一個按鍵都沒送）" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -Fxq "$2" "$1"' _ "$TESTROOT/ask2-got.txt" 'SENTINEL-ask2'
assert "對話框偵測(Edit 框)：events.log 記 notify-failed" \
  evt_grep "$D5c/tasks/$id5c/events.log" notify-failed
tmx kill-pane -t "$p_edit" 2>/dev/null || true

# 8a③ 放行對照：畫面無對話框特徵 → 通知照送，pane 確實收到通知文字（正向鎖住
# 「不誤判把活 worker 打死」，且直接驗按鍵送達而非只驗沒記 notify-failed）
D5b="$TESTROOT/d5b"
p_ok="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D5b" register momo "$p_ok" 2>/dev/null
tmx send-keys -t "$p_ok" \
  "printf '%s\n' 'just some normal worker output' ; touch $TESTROOT/ok-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/ok-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/ok-ready"
id5b="$(ab "$D5b" send momo --from alice --message hi 2>/dev/null)"
assert "非對話框畫面：pane 收到通知文字（通知照送）" \
  wait_for 10 grep -q "$id5b" "$TESTROOT/ok-got.txt"
assert_fails "非對話框畫面：通知未降級（events.log 不記 notify-failed）" \
  evt_grep "$D5b/tasks/$id5b/events.log" notify-failed
tmx kill-pane -t "$p_ok" 2>/dev/null || true

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

# ---- 16. spawn：核心＋cap＋原子回滾（Phase 1） ----

# 16a. 快樂路徑：假 codex 啟動期吃輸入 2 秒，首發探針必被吃，
# ready 仍翻 true ＝ 探針重送機制生效（Phase 2 gate 一併覆蓋）
pane_w1="$(absp "$DSPAWN" 20 spawn w1 --runtime codex 2>"$TESTROOT/spawn-w1.err")"; rc=$?
assert "spawn 成功：exit 0" test "$rc" -eq 0
assert "spawn stdout 只印 pane-id（%N）一行" \
  bash -c "[[ '$pane_w1' =~ ^%[0-9]+\$ ]]"
assert "spawn 的 pane 存在於 tmux" pane_alive "$pane_w1"
assert "spawn registry：spawned/runtime/spawned_at 欄位齊" \
  jq -e '.spawned == true and .runtime == "codex" and (.spawned_at | type == "string")' \
  "$DSPAWN/agents/w1.json"
assert "spawn 以 --profile agent-worker 啟動 runtime" \
  grep -q -- '--profile agent-worker' "$TESTROOT/codex-args.txt"
assert "spawn 等到 ready：registry ready == true（探針重送生效）" \
  jq -e '.ready == true' "$DSPAWN/agents/w1.json"
assert "agents.log 記 spawned w1" \
  grep -qE "Z spawned w1 ${pane_w1} codex\$" "$DSPAWN/agents.log"
assert "list：spawned＋就緒 agent ready 欄為 ready" \
  list_has "$DSPAWN" "$(printf 'w1\t%s\tready' "$pane_w1")"
assert "spawn 後 locks 無殘留" test -z "$(ls -A "$DSPAWN/locks" 2>/dev/null)"
# 回滾靠 pane_start_command 認 tag（見 spawn_rollback）。tmux 會把含空格的
# 啟動指令用雙引號包起來存（實測 3.7b），比對前必須剝得掉那層引號——鎖住
# 這個不變量，否則 tmux 改存法時會變成默默漏殺孤兒 pane
w1_cmd="$(tmx display -pt "$pane_w1" '#{pane_start_command}')"
assert "pane_start_command 剝前導引號後以 spawn tag 前綴開頭（tmux 行為不變量）" \
  bash -c "c=${w1_cmd@Q}; c=\"\${c#\\\"}\"; [[ \"\$c\" == AGENT_BRIDGE_SPAWN_TAG=ab-spawn-* ]]"

# 16a2. claude runtime：注入形狀與 codex 同（實測 Claude Code 會把位置參數
# 當第一則 user message 執行、且執行完 session 常駐收得到探針），差別只在
# 啟動旗標。這裡鎖住的是那些旗標的選擇理由，不是形狀：
#   - `--permission-mode auto` 換成 bypassPermissions 是安全降級：官方把後者
#     定位為僅限隔離容器／VM，而 worker pane 跑在本機、與主 session 共用檔案
#     系統與憑證。（理由到此為止——bypass 並不停用 hooks，deny 與 explicit ask
#     也仍適用；詳見 README，那段因果曾兩度寫錯）
#   - 混進 -p/--print 會讓 pane 跑完即退，worker 根本不存在
#   - 混進 --settings/--setting-sources 會讓 worker 脫離使用者的安全設定
# 關鍵是**精確的完整參數集合**而非子字串：獨立複核以 mutation 反例證明，
# 只比對子字串時 `--permission-mode auto --permission-mode bypassPermissions
# --setting-sources project <prompt>` 這種 argv 能讓所有斷言全綠——後面偷偷
# 追加的旗標才是真正生效的那個。白名單式斷言（恰好三個參數）比逐條列黑名單
# 更強：任何多餘旗標都會讓行數對不上。
# 測完立刻 despawn，避免佔用 DSPAWN 的 spawn cap 影響後續測例
pane_wc="$(absp "$DSPAWN" 20 spawn wc1 --runtime claude 2>/dev/null)"; rc=$?
assert "spawn --runtime claude：exit 0" test "$rc" -eq 0
assert "claude runtime registry：runtime 欄為 claude" \
  jq -e '.runtime == "claude"' "$DSPAWN/agents/wc1.json"
assert "claude runtime 以 --permission-mode auto 啟動" \
  grep -q -- '--permission-mode auto' "$TESTROOT/claude-args.txt"
# 否定式斷言必須先證明「有東西可否定」：spawn 若整個失敗，args 檔根本不存在，
# 光 grep 找不到 -p 也會綠——那是空綠，什麼都沒鎖住（這個 repo 踩過的坑）
assert "claude runtime 不帶 -p/--print（headless 會讓 pane 跑完即退）" \
  bash -c "[[ -s '$TESTROOT/claude-args.txt' ]] && ! grep -qE -- '(^| )(-p|--print)( |\$)' '$TESTROOT/claude-args.txt'"
assert "claude runtime 一樣注入 worker brief（走同一條 worker_prompt_arg）" \
  grep -q -- '以上是你的 worker 守則' "$TESTROOT/claude-args.txt"
# 白名單：argv 必須「恰好」是 --permission-mode / auto / <prompt> 三個。
# 這條才是真正擋住「追加旗標」的防線，上面幾條子字串斷言擋不住
assert "claude runtime argv 恰好三個參數（追加任何旗標都該紅）" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/claude-argv.txt'; (( \${#A[@]} == 3 ))"
assert "claude runtime argv 前兩個恰為 --permission-mode auto、第三個是 prompt 非旗標" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/claude-argv.txt'; [[ \${A[0]} == '--permission-mode' && \${A[1]} == 'auto' && \${A[2]} != -* ]]"
assert "claude runtime 探針重送生效：ready == true" \
  jq -e '.ready == true' "$DSPAWN/agents/wc1.json"
assert "agents.log 記 spawned wc1 … claude" \
  grep -qE "Z spawned wc1 ${pane_wc} claude\$" "$DSPAWN/agents.log"
assert "claude runtime worker 可正常 despawn" ab "$DSPAWN" despawn wc1

# 16a3. spawn --model：模型下放。值會進 tagged_cmd（pane 啟動命令字串），
# 與 brief 路徑同級暴露面，防線兩條：字元集擋 sh/tmux 分隔符（命令注入）、
# 首字元強制英數（旗標走私——`--model --bare` 不擋的話就是往 worker 啟動
# 旗標塞任意開關的後門）。argv 斷言沿用 16a2 的教訓走白名單：鎖「恰好五個
# 參數」，子字串斷言擋不住偷偷追加的旗標
absp "$DSPAWN" 20 spawn wm1 --runtime claude --model sonnet-t.0 >/dev/null 2>&1; rc=$?
assert "spawn --model：exit 0" test "$rc" -eq 0
assert "claude argv 恰好五個參數（--permission-mode auto --model <m> <prompt>）" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/claude-argv.txt'; (( \${#A[@]} == 5 ))"
assert "claude argv 的 --model 值就位、prompt 仍是最後一個非旗標參數" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/claude-argv.txt'; [[ \${A[2]} == '--model' && \${A[3]} == 'sonnet-t.0' && \${A[4]} != -* ]]"
assert "registry 記下 model 欄位" \
  jq -e '.model == "sonnet-t.0"' "$DSPAWN/agents/wm1.json"
assert "spawn --model 的 worker 可正常 despawn" ab "$DSPAWN" despawn wm1

# codex 側走同一條 append 路徑
absp "$DSPAWN" 20 spawn wm2 --runtime codex --model gpt-test >/dev/null 2>&1; rc=$?
assert "spawn --runtime codex --model：exit 0" test "$rc" -eq 0
assert "codex args 帶相鄰的 --model gpt-test" \
  grep -q -- '--model gpt-test' "$TESTROOT/codex-args.txt"
assert "codex --model worker 可正常 despawn" ab "$DSPAWN" despawn wm2

# 未指定 --model：model 欄為空字串＝沿用 runtime 預設；argv 形狀由 16a2 的
# 「恰好三個參數」白名單鎖住，這裡不重複（w1 是 16a 留下的無 --model spawn）
assert "未指定 --model 時 registry model 欄為空字串" \
  jq -e '.model == ""' "$DSPAWN/agents/w1.json"

# 不合法的值必須在建 pane 之前死：pane 數不變、registry 不落地。
# 「pane 數不變」先取樣再操作，斷言的是被拒路徑真的沒碰 tmux
pc_before="$(pane_count)"
assert_fails "拒絕含分隔符的 model（x;kill-server）" \
  ab "$DSPAWN" spawn wm3 --runtime claude --model 'x;kill-server'
assert_fails "拒絕含空白的 model" \
  ab "$DSPAWN" spawn wm3 --runtime claude --model 'x y'
assert_fails "拒絕以 - 起首的 model（旗標走私：--model --bare）" \
  ab "$DSPAWN" spawn wm3 --runtime claude --model --bare
assert_fails "拒絕空字串 model" \
  ab "$DSPAWN" spawn wm3 --runtime claude --model ''
assert_fails "拒絕超長 model（65 字元）" \
  ab "$DSPAWN" spawn wm3 --runtime claude --model "$(printf 'a%.0s' {1..65})"
assert_fails "--model 缺值時報錯" \
  ab "$DSPAWN" spawn wm3 --runtime claude --model
assert "被拒的 spawn 不建 pane（pane 數不變）" \
  test "$(pane_count)" -eq "$pc_before"
assert "被拒的 spawn 不留 registry" \
  bash -c "[[ ! -e '$DSPAWN/agents/wm3.json' ]]"
assert_fails "despawn 後 claude worker 的 pane 消失" pane_alive "$pane_wc"

# 16b. 參數與名稱衝突拒絕
assert_fails "spawn 已註冊（spawned）名稱被拒" absp "$DSPAWN" 0 spawn w1 --runtime codex
ab "$DSPAWN" register manual-x "$PANE_A" 2>/dev/null
assert_fails "spawn 與人工註冊同名被拒" absp "$DSPAWN" 0 spawn manual-x --runtime codex
assert_fails "spawn：不支援的 runtime 被拒" absp "$DSPAWN" 0 spawn wx --runtime gemini
assert "不支援 runtime：不留 registry" test ! -e "$DSPAWN/agents/wx.json"
assert_fails "spawn 缺 --runtime 被拒" absp "$DSPAWN" 0 spawn wy
assert_fails "spawn：名稱不合法被拒" absp "$DSPAWN" 0 spawn "bad name" --runtime codex
assert "list：人工 agent ready 欄為 -" \
  list_has "$DSPAWN" "$(printf 'manual-x\t%s\t-' "$PANE_A")"

# 16c. cap：上限、人工註冊不計入、並行 spawn 不繞過
DCAP="$TESTROOT/dcap"
abcap() {
  env AGENT_BRIDGE_DATA="$DCAP" AGENT_BRIDGE_MAX_SPAWN=2 \
      AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" "$BRIDGE" "$@"
}
abcap spawn c1 --runtime codex >/dev/null 2>&1
abcap spawn c2 --runtime codex >/dev/null 2>&1
assert "cap：上限內 spawn 成功（2/2）" bash -c \
  "test -f '$DCAP/agents/c1.json' && test -f '$DCAP/agents/c2.json'"
before_cap="$(pane_count)"
abcap spawn c3 --runtime codex >/dev/null 2>"$TESTROOT/cap.err"; rc=$?
assert "cap 超限：非零退出" test "$rc" -ne 0
assert "cap 超限：訊息含 AGENT_BRIDGE_MAX_SPAWN" grep -q 'AGENT_BRIDGE_MAX_SPAWN' "$TESTROOT/cap.err"
assert "cap 超限：不留 registry" test ! -e "$DCAP/agents/c3.json"
assert "cap 超限：pane 數不變" test "$(pane_count)" -eq "$before_cap"
# 人工註冊不計入 cap：despawn 一個後，即使多一個人工 agent 仍可 spawn
ab "$DCAP" register human "$PANE_A" 2>/dev/null
abcap despawn c2 >/dev/null 2>&1
abcap spawn c4 --runtime codex >/dev/null 2>&1; rc=$?
assert "cap：人工註冊不計入 spawned 上限" test "$rc" -eq 0
abcap despawn c1 >/dev/null 2>&1
abcap despawn c4 >/dev/null 2>&1

# 並行 4 個 spawn 搶 cap=2：registry 鎖序列化下恰 2 個得手、無殘鎖
DCON="$TESTROOT/dcon"
before_con="$(pane_count)"
for i in 1 2 3 4; do
  env AGENT_BRIDGE_DATA="$DCON" AGENT_BRIDGE_MAX_SPAWN=2 \
      AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
      "$BRIDGE" spawn "r$i" --runtime codex >/dev/null 2>&1 &
done
wait
assert "並行 spawn 搶 cap：恰 2 個註冊成功" \
  test "$(find "$DCON/agents" -name '*.json' | wc -l)" -eq 2
assert "並行 spawn 搶 cap：pane 恰增加 2 個" \
  test "$(pane_count)" -eq "$((before_con + 2))"
assert "並行 spawn 搶 cap：locks 無殘留" test -z "$(ls -A "$DCON/locks" 2>/dev/null)"
for f in "$DCON"/agents/*.json; do
  ab "$DCON" despawn "$(jq -r '.name' "$f")" >/dev/null 2>&1
done

# 16d. 失敗注入零殘留：runtime 啟動即死、註冊寫入失敗（回滾 kill pane）
DRB="$TESTROOT/drb"
ab "$DRB" list >/dev/null 2>&1   # 先建出資料目錄
touch "$TESTROOT/codex-fail"
before_nf="$(pane_count)"
absp "$DRB" 0 spawn nf --runtime codex >/dev/null 2>"$TESTROOT/nf.err"; rc=$?
rm -f "$TESTROOT/codex-fail"
assert "runtime 啟動即失敗：非零退出" test "$rc" -ne 0
assert "runtime 啟動即失敗：stderr 說明" grep -q '啟動即失敗' "$TESTROOT/nf.err"
assert "runtime 啟動即失敗：不留 registry" test ! -e "$DRB/agents/nf.json"
assert "runtime 啟動即失敗：pane 數不變" test "$(pane_count)" -eq "$before_nf"
assert "runtime 啟動即失敗：locks 無殘留" test -z "$(ls -A "$DRB/locks" 2>/dev/null)"

chmod 555 "$DRB/agents"
before_rb="$(pane_count)"
absp "$DRB" 0 spawn rb --runtime codex >/dev/null 2>&1; rc=$?
chmod 755 "$DRB/agents"
assert "註冊寫入失敗：非零退出" test "$rc" -ne 0
assert "註冊寫入失敗：回滾 kill 已建 pane（pane 數不變）" \
  test "$(pane_count)" -eq "$before_rb"
assert "註冊寫入失敗：不留 registry" test ! -e "$DRB/agents/rb.json"
assert "註冊寫入失敗：locks 無殘留" test -z "$(ls -A "$DRB/locks" 2>/dev/null)"
assert "失敗注入：agents.log 無 spawned 紀錄" bash -c \
  "! grep -qE 'Z spawned (nf|rb) ' '$DRB/agents.log' 2>/dev/null"

# --window 變體：pane 開在新 window
pane_ww="$(absp "$DSPAWN" 0 spawn ww --runtime codex --window 2>/dev/null)"
assert "spawn --window：pane 存在" pane_alive "$pane_ww"
assert "spawn --window：與既有 pane 不同 window" bash -c \
  "[[ \"\$($REAL_TMUX -L $SOCK display -pt '$pane_ww' '#{window_id}')\" != \
     \"\$($REAL_TMUX -L $SOCK display -pt '$PANE_A' '#{window_id}')\" ]]"
ab "$DSPAWN" despawn ww >/dev/null 2>&1

# ---- 17. ready／探針（Phase 2） ----
pane_w5="$(absp "$DSPAWN" 0 spawn w5 --runtime codex 2>/dev/null)"
assert "READY_TIMEOUT=0：spawn 立即返回、ready 仍 false" \
  jq -e '.ready == false' "$DSPAWN/agents/w5.json"
assert "list：未就緒 spawned agent 欄為 starting" \
  list_has "$DSPAWN" "$(printf 'w5\t%s\tstarting' "$pane_w5")"
idw5="$(ab "$DSPAWN" send w5 --from alice --message hi 2>"$TESTROOT/w5-send.err")"; rc=$?
assert "send 未就緒 agent：仍 exit 0（不拒送）" test "$rc" -eq 0
assert "send 未就緒 agent：stderr 印警告" grep -q '尚未回報就緒' "$TESTROOT/w5-send.err"
assert "send 未就緒 agent：任務照建" test -d "$DSPAWN/tasks/$idw5"
ab "$DSPAWN" ready w5 2>/dev/null; rc=$?
assert "ready：手動回報 exit 0" test "$rc" -eq 0
assert "ready：registry 翻 true" jq -e '.ready == true' "$DSPAWN/agents/w5.json"
assert "list：ready 後欄位變 ready" \
  list_has "$DSPAWN" "$(printf 'w5\t%s\tready' "$pane_w5")"
assert_fails "ready：人工 agent 被拒" ab "$DSPAWN" ready manual-x
assert_fails "ready：未註冊 agent 報錯" ab "$DSPAWN" ready ghost
assert_fails "ready：名稱不合法被拒" ab "$DSPAWN" ready "bad name"

# 就緒逾時：慢到 600 秒的假 codex，timeout 1s → 僅警告、pane 留用、不回滾
echo 600 > "$TESTROOT/codex-delay"
pane_w6="$(absp "$DSPAWN" 1 spawn w6 --runtime codex 2>"$TESTROOT/w6.err")"; rc=$?
echo 2 > "$TESTROOT/codex-delay"
assert "就緒逾時：exit 0（僅警告不回滾）" test "$rc" -eq 0
assert "就緒逾時：stderr 印未回報就緒警告" grep -q '未回報就緒' "$TESTROOT/w6.err"
assert "就緒逾時：pane 留用供診斷" pane_alive "$pane_w6"
assert "就緒逾時：registry 仍在、ready false" jq -e '.ready == false' "$DSPAWN/agents/w6.json"

# ---- 18. despawn＋出身防護（Phase 3） ----
assert_fails "despawn 人工 agent 被拒" ab "$DSPAWN" despawn manual-x
assert "despawn 被拒：registry 檔仍在" test -f "$DSPAWN/agents/manual-x.json"
ab "$DSPAWN" despawn w1 2>/dev/null; rc=$?
assert "despawn spawned agent：exit 0" test "$rc" -eq 0
assert_fails "despawn 後 pane 消失" pane_alive "$pane_w1"
assert "despawn 後 registry 檔已刪" test ! -e "$DSPAWN/agents/w1.json"
# 沒宣告過 disposable＝仍被視為有殘值，直接 despawn 等於繞過收尾流程。
# 機制上不擋，但審計要看得出這次回收沒有筆記（見 cmd_despawn 的 ev 判定）
assert "agents.log 記 despawned-unsaved w1（未宣告 disposable，繞過了收尾）" \
  grep -qE "Z despawned-unsaved w1 ${pane_w1} codex\$" "$DSPAWN/agents.log"
assert_fails "despawn 未註冊 agent 報錯" ab "$DSPAWN" despawn w1
assert_fails "despawn：名稱不合法被拒" ab "$DSPAWN" despawn "bad name"

# 死 pane（如 tmux server 重啟）despawn 仍清 registry
pane_w7="$(absp "$DSPAWN" 0 spawn w7 --runtime codex 2>/dev/null)"
tmx kill-pane -t "$pane_w7"
ab "$DSPAWN" despawn w7 2>/dev/null; rc=$?
assert "死 pane despawn：exit 0" test "$rc" -eq 0
assert "死 pane despawn：registry 已清" test ! -e "$DSPAWN/agents/w7.json"
assert "死 pane despawn：agents.log 記下這次回收（未宣告 disposable → unsaved）" \
  grep -qE "Z despawned-unsaved w7 " "$DSPAWN/agents.log"

# ---- 18b. despawn 的出身證據＝pane 啟動指令的 tag（codex 第三輪 FAIL 1） ----
# pane id 會被 tmux 重用（不同 server／server 重啟後計數器歸零），registry 也
# 可能被有資料目錄寫入權的 worker 偽造。只憑 pane_id 相符就殺，會殺到人工 pane。
DTAG="$TESTROOT/dtag"
pane_t="$(absp "$DTAG" 0 spawn tagged --runtime codex 2>/dev/null)"
assert "spawn：registry 存下 spawn_tag" \
  jq -e '.spawn_tag | startswith("AGENT_BRIDGE_SPAWN_TAG=ab-spawn-")' "$DTAG/agents/tagged.json"

# 情境一：registry 指向一個「存在但不是我們開的」pane（模擬 id 重用／偽造）
pane_innocent="$(tmx split-window -dP -F '#{pane_id}' "$pane_cmd")"
jq --arg p "$pane_innocent" '.pane_id = $p' "$DTAG/agents/tagged.json" > "$TESTROOT/tg.tmp"
mv "$TESTROOT/tg.tmp" "$DTAG/agents/tagged.json"
ab "$DTAG" despawn tagged >/dev/null 2>"$TESTROOT/tag-stale.err"; rc=$?
assert "tag 不符：despawn 不殺該 pane（人工 pane 存活）" pane_alive "$pane_innocent"
assert "tag 不符：stderr 明確警告未動該 pane" \
  grep -q '未動該 pane' "$TESTROOT/tag-stale.err"
assert "tag 不符：仍清除過時註冊" test ! -e "$DTAG/agents/tagged.json"
assert "tag 不符：exit 0（清理視為完成）" test "$rc" -eq 0
assert "tag 不符：audit 記 despawn-stale" \
  grep -qE "Z despawn-stale tagged " "$DTAG/agents.log"
assert "tag 不符：locks 無殘留" test -z "$(ls -A "$DTAG/locks" 2>/dev/null)"
tmx kill-pane -t "$pane_innocent" 2>/dev/null
tmx kill-pane -t "$pane_t" 2>/dev/null

# 情境二（codex 第四輪 FAIL 1）：registry 在 worker 的可寫範圍內，所以 tag 本身
# 也必須驗格式與名字綁定，否則它只是個「攻擊者說了算」的欄位。
# 弱偽造：spawn_tag 填 "bash"，指向一個跑 bash 的人工 pane，前綴比對就會命中
DFORGE="$TESTROOT/dforge"
# 啟動指令必須真的以 bash 開頭，才吃得到 spawn_tag="bash" 這個萬用鑰匙
pane_bash="$(tmx split-window -dP -F '#{pane_id}' 'bash --norc --noprofile')"
mkdir -p "$DFORGE/agents" "$DFORGE/locks" "$DFORGE/tasks"
jq -n --arg p "$pane_bash" \
  '{name: "forged", pane_id: $p, registered_at: "2026-01-01T00:00:00Z",
    spawned: true, runtime: "codex", spawned_at: "2026-01-01T00:00:00Z",
    ready: true, spawn_tag: "bash"}' \
  > "$DFORGE/agents/forged.json"
ab "$DFORGE" despawn forged >/dev/null 2>"$TESTROOT/forge.err"; rc=$?
assert "偽造短 tag：人工 pane 未被殺" pane_alive "$pane_bash"
assert "偽造短 tag：走 stale 分支（stderr 警告未動該 pane）" \
  grep -q '未動該 pane' "$TESTROOT/forge.err"
assert "偽造短 tag：exit 0（僅清除註冊）" test "$rc" -eq 0

# 跨 worker 抄 tag：worker A 把 B 的 pane_id 與 spawn_tag 抄進自己的 registry，
# 誘使 orchestrator 的 `despawn A` 殺掉 B 的 pane。tag 綁名字才擋得住
DCOPY="$TESTROOT/dcopy"
pane_b="$(absp "$DCOPY" 0 spawn bb --runtime codex 2>/dev/null)"
b_tag="$(jq -r '.spawn_tag' "$DCOPY/agents/bb.json")"
jq -n --arg p "$pane_b" --arg t "$b_tag" \
  '{name: "aa", pane_id: $p, registered_at: "2026-01-01T00:00:00Z",
    spawned: true, runtime: "codex", spawned_at: "2026-01-01T00:00:00Z",
    ready: true, spawn_tag: $t}' > "$DCOPY/agents/aa.json"
ab "$DCOPY" despawn aa >/dev/null 2>"$TESTROOT/copy.err"; rc=$?
assert "抄別人的 tag：B 的 pane 未被殺" pane_alive "$pane_b"
assert "抄別人的 tag：走 stale 分支" grep -q '未動該 pane' "$TESTROOT/copy.err"
assert "抄別人的 tag：B 的 registry 未受影響" test -f "$DCOPY/agents/bb.json"
assert "正常 despawn B 仍殺得掉（tag 綁名字沒有誤傷正常路徑）" \
  bash -c "env AGENT_BRIDGE_DATA=$(printf '%q' "$DCOPY") PATH=$(printf '%q' "$SHIM:$PATH") \
    $(printf '%q' "$BRIDGE") despawn bb >/dev/null 2>&1"
assert_fails "正常 despawn B：pane 確實消失" pane_alive "$pane_b"
tmx kill-pane -t "$pane_bash" 2>/dev/null

# 情境二之二：pane_id 也是不可信的 registry 欄位，而它會進 tmux 命令字串
# （if-shell 的 kill-pane）。tmux 命令裡 `;` 是分隔符，不驗格式就是命令注入
DINJ="$TESTROOT/dinj"
mkdir -p "$DINJ/agents" "$DINJ/locks" "$DINJ/tasks"
jq -n '{name: "inj", pane_id: "%0 ; kill-server", registered_at: "2026-01-01T00:00:00Z",
        spawned: true, runtime: "codex", spawned_at: "2026-01-01T00:00:00Z",
        ready: true, spawn_tag: "AGENT_BRIDGE_SPAWN_TAG=ab-spawn-inj-1-aabbccddeeff"}' \
  > "$DINJ/agents/inj.json"
inj_before="$(pane_count)"
ab "$DINJ" despawn inj >/dev/null 2>"$TESTROOT/inj.err"; rc=$?
assert "pane_id 注入：非零退出" test "$rc" -ne 0
assert "pane_id 注入：stderr 說明格式不合法" \
  grep -q 'pane_id 格式不合法' "$TESTROOT/inj.err"
assert "pane_id 注入：tmux server 存活、pane 數不變" \
  test "$(pane_count)" -eq "$inj_before"
assert "pane_id 注入：registry 保留（不因竄改而靜默清除）" test -f "$DINJ/agents/inj.json"

# 情境三（codex 第四輪 FAIL 2）：驗證與 kill 之間 tmux server 被換掉。
# shim 讓 list-panes 走 socket A（有合法 worker），if-shell/kill 走 socket B
# （同一個 %N 是人工 pane）→ 原子 if-shell 在 B 上判 false，不得殺 B 的 pane
SOCKB="agent-bridge-test-b"
tmxb() { "$REAL_TMUX" -L "$SOCKB" -f /dev/null "$@"; }
tmxb new-session -d -s b -x 200 -y 100 "$pane_cmd"
mapfile -t BPANES < <(tmxb list-panes -a -F '#{pane_id}')
PANE_B0="${BPANES[0]}"
DSWAP="$TESTROOT/dswap"
pane_s="$(absp "$DSWAP" 0 spawn sw --runtime codex 2>/dev/null)"
# 把 registry 的 pane_id 改成 B server 上那個人工 pane 的 id，模擬「id 相同、
# 但第二次連線落到另一個 server」
jq --arg p "$PANE_B0" '.pane_id = $p' "$DSWAP/agents/sw.json" > "$TESTROOT/sw.tmp"
mv "$TESTROOT/sw.tmp" "$DSWAP/agents/sw.json"
SWAPSHIM="$TESTROOT/swapshim"
mkdir -p "$SWAPSHIM"
cat > "$SWAPSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
# list-panes 回答 A server 的舊快照（偽造成目標 pane 帶著合法 tag），
# 其餘呼叫（if-shell / 存活檢查）落到 B server
if [[ "\$1" == "list-panes" && "\$*" == *pane_start_command* ]]; then
  printf '%s "%s exec codex --profile agent-worker"\n' \
    $(printf '%q' "$PANE_B0") "\$(jq -r '.spawn_tag' $(printf '%q' "$DSWAP/agents/sw.json"))"
  exit 0
fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCKB") -f /dev/null "\$@"
EOF
chmod +x "$SWAPSHIM/tmux"
ln -s "$BRIDGE" "$SWAPSHIM/agent-bridge"
env AGENT_BRIDGE_DATA="$DSWAP" PATH="$SWAPSHIM:$PATH" "$BRIDGE" despawn sw \
  >/dev/null 2>"$TESTROOT/swap.err"; rc=$?
assert "server 中途替換：另一 server 的同 id 人工 pane 未被殺" \
  bash -c "$(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCKB") -f /dev/null \
    list-panes -a -F '#{pane_id}' | grep -Fxq $(printf '%q' "$PANE_B0")"
assert "server 中途替換：非零退出" test "$rc" -ne 0
assert "server 中途替換：registry 保留不動" test -f "$DSWAP/agents/sw.json"
assert "server 中途替換：stderr 說明無法關閉 pane" \
  grep -q '無法關閉 pane' "$TESTROOT/swap.err"
"$REAL_TMUX" -L "$SOCKB" -f /dev/null kill-server 2>/dev/null || true
tmx kill-pane -t "$pane_s" 2>/dev/null

# ---- 18c. despawn 不得把 tmux 失敗當成「pane 已消失」（codex 第三輪 FAIL 2） ----
DDSP="$TESTROOT/ddsp"
pane_k="$(absp "$DDSP" 0 spawn kx --runtime codex 2>/dev/null)"

# 查詢失敗：registry 必須原封不動，否則 pane 變成沒人回收的孤兒
ab_notmux "$DDSP" despawn kx >/dev/null 2>"$TESTROOT/dsp-notmux.err"; rc=$?
assert "list-panes 失敗：非零退出" test "$rc" -ne 0
assert "list-panes 失敗：registry 保留不動" test -f "$DDSP/agents/kx.json"
assert "list-panes 失敗：pane 仍在" pane_alive "$pane_k"
assert "list-panes 失敗：stderr 說明 registry 未動" \
  grep -qE '無法查詢 tmux pane|找不到 tmux' "$TESTROOT/dsp-notmux.err"
assert "list-panes 失敗：不寫 despawned audit" \
  bash -c "! grep -qE 'Z despawned kx ' '$DDSP/agents.log' 2>/dev/null"

# kill 沒生效：同樣不得刪 registry。kill 現在由 tmux 在 if-shell 內部執行
# （原子驗證，見 cmd_despawn），所以攔 if-shell 就等於「驗證沒過／kill 沒發生」
KILLSHIM="$TESTROOT/killshim"
mkdir -p "$KILLSHIM"
cat > "$KILLSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
if [[ "\$1" == "if-shell" ]]; then exit 0; fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$KILLSHIM/tmux"
ln -s "$BRIDGE" "$KILLSHIM/agent-bridge"
env AGENT_BRIDGE_DATA="$DDSP" PATH="$KILLSHIM:$PATH" "$BRIDGE" despawn kx \
  >/dev/null 2>"$TESTROOT/dsp-kill.err"; rc=$?
assert "kill 未生效：非零退出" test "$rc" -ne 0
assert "kill 未生效：registry 保留不動" test -f "$DDSP/agents/kx.json"
assert "kill 未生效：pane 仍在" pane_alive "$pane_k"
assert "kill 未生效：stderr 說明無法關閉 pane" \
  grep -q '無法關閉 pane' "$TESTROOT/dsp-kill.err"
assert "kill 未生效：locks 無殘留" test -z "$(ls -A "$DDSP/locks" 2>/dev/null)"
ab "$DDSP" despawn kx >/dev/null 2>&1   # 正常路徑收尾
assert "排除障礙後 despawn 正常完成" test ! -e "$DDSP/agents/kx.json"

ab "$DSPAWN" despawn w5 >/dev/null 2>&1
ab "$DSPAWN" despawn w6 >/dev/null 2>&1
assert "spawn/despawn 全程後 locks 無殘留" \
  test -z "$(ls -A "$DSPAWN/locks" 2>/dev/null)"

# ---- 19. codex 複核回歸：出身防護的 TOCTOU 與回滾漏洞 ----

# 19a. provenance race（codex FAIL 1）：出身檢查若在鎖外，despawn 可能先驗過
# 舊的 spawned 紀錄、再殺到同名人工註冊的 pane。兩道防線分開驗。

# 防線一：register 不得覆寫 spawned agent（同名替換這條路整個封死）
DRACE="$TESTROOT/drace"
pane_v="$(absp "$DRACE" 0 spawn victim --runtime codex 2>/dev/null)"
pane_human="$(tmx split-window -dP -F '#{pane_id}' "$pane_cmd")"
ab "$DRACE" register victim "$pane_human" >/dev/null 2>&1; rc=$?
assert "register 拒絕覆寫 spawned agent" test "$rc" -ne 0
# shellcheck disable=SC2016  # $p 是 jq 變數（由 --arg 傳入），非 shell 展開
assert "register 被拒：registry 仍是 spawned 紀錄且 pane 未變" \
  jq -e --arg p "$pane_v" '.spawned == true and .pane_id == $p' "$DRACE/agents/victim.json"

# 防線二：出身檢查確實在鎖內——趁 despawn 等鎖時直接把 registry 換成人工紀錄
# （繞過 register，模擬任何未來的替換途徑），放鎖後 despawn 必須拒殺
mkdir -p "$DRACE/locks/agents-registry.lock"      # 卡住 despawn，製造窗口
ab "$DRACE" despawn victim >/dev/null 2>"$TESTROOT/race-despawn.err" &
race_pid=$!
sleep 0.5
jq -n --arg p "$pane_human" \
  '{name: "victim", pane_id: $p, registered_at: "2026-01-01T00:00:00Z"}' \
  > "$DRACE/agents/victim.json"
rmdir "$DRACE/locks/agents-registry.lock"
wait "$race_pid"; rc=$?
assert "鎖內出身檢查：registry 於等鎖期間被換成人工紀錄 → despawn 非零退出" \
  test "$rc" -ne 0
assert "鎖內出身檢查：stderr 說明非 spawn 出身" \
  grep -q '非 spawn 出身' "$TESTROOT/race-despawn.err"
assert "鎖內出身檢查：人工 pane 未被誤殺" pane_alive "$pane_human"
assert "鎖內出身檢查：人工 registry 未被刪" test -f "$DRACE/agents/victim.json"
assert "鎖內出身檢查：locks 無殘留" \
  test -z "$(ls -A "$DRACE/locks" 2>/dev/null)"
tmx kill-pane -t "$pane_human" 2>/dev/null
tmx kill-pane -t "$pane_v" 2>/dev/null
rm -f "$DRACE/agents/victim.json"

# unregister 不得把 spawned agent 除名（會留下沒人認領的 pane、cap 少算）
pane_u="$(absp "$DRACE" 0 spawn uw --runtime codex 2>/dev/null)"
assert_fails "unregister 拒絕移除 spawned agent" ab "$DRACE" unregister uw
assert "unregister 被拒：registry 仍在" test -f "$DRACE/agents/uw.json"
assert "unregister 被拒：pane 仍在" pane_alive "$pane_u"
ab "$DRACE" despawn uw >/dev/null 2>&1

# 19b. pane 已建但 pane id 沒回到手上（codex FAIL 2）：
# tmux shim 照常建 pane 卻丟棄 -P 輸出 → 回滾必須靠啟動指令裡的 tag 掃出孤兒
GAPSHIM="$TESTROOT/gapshim"
mkdir -p "$GAPSHIM"
cat > "$GAPSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
if [[ "\$1" == "split-window" || "\$1" == "new-window" ]]; then
  $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@" >/dev/null
  exit 0
fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$GAPSHIM/tmux"
ln -s "$BRIDGE" "$GAPSHIM/agent-bridge"
ln -s "$SHIM/codex" "$GAPSHIM/codex"
DGAP="$TESTROOT/dgap"
before_gap="$(pane_count)"
env AGENT_BRIDGE_DATA="$DGAP" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$GAPSHIM:$PATH" \
  "$BRIDGE" spawn gap --runtime codex >/dev/null 2>"$TESTROOT/gap.err"; rc=$?
assert "pane id 遺失：非零退出" test "$rc" -ne 0
assert "pane id 遺失：stderr 說明未回傳 pane id" \
  grep -q '未回傳 pane id' "$TESTROOT/gap.err"
assert "pane id 遺失：不留 registry" test ! -e "$DGAP/agents/gap.json"
assert "pane id 遺失：孤兒 pane 被 tag 掃出並回收（pane 數不變）" \
  test "$(pane_count)" -eq "$before_gap"
assert "pane id 遺失：locks 無殘留" test -z "$(ls -A "$DGAP/locks" 2>/dev/null)"

# 19b'. tag 比對必須錨在啟動指令開頭（codex 第二輪 FAIL 1）：
# 旁邊擺一個「參數裡碰巧含同一 tag 字串」的無關 pane，回滾不得誤殺它。
# shim 把本次 tagged_cmd 寫出來，測試據此造出對照 pane 後才放行。
BAITSHIM="$TESTROOT/baitshim"
mkdir -p "$BAITSHIM"
cat > "$BAITSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
TMX=($(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null)
if [[ "\$1" == "split-window" || "\$1" == "new-window" ]]; then
  for a in "\$@"; do :; done
  printf '%s' "\$a" > "$TESTROOT/bait-cmd.txt"
  # 對照 pane：命令中段含同一 tag 子字串，但不是以它開頭
  "\${TMX[@]}" split-window -d "sleep 600 # bait \$a tail"
  "\${TMX[@]}" "\$@" >/dev/null
  exit 0
fi
exec "\${TMX[@]}" "\$@"
EOF
chmod +x "$BAITSHIM/tmux"
ln -s "$BRIDGE" "$BAITSHIM/agent-bridge"
ln -s "$SHIM/codex" "$BAITSHIM/codex"
DBAIT="$TESTROOT/dbait"
before_bait="$(pane_count)"
env AGENT_BRIDGE_DATA="$DBAIT" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$BAITSHIM:$PATH" \
  "$BRIDGE" spawn bait --runtime codex >/dev/null 2>&1; rc=$?
assert "tag 比對：pane id 遺失情境仍非零退出" test "$rc" -ne 0
# 起始 + 對照 pane + 目標 pane = before+2；回滾只該收掉目標 pane
assert "tag 比對：只回收自己的 pane，對照 pane 存活（淨增 1）" \
  test "$(pane_count)" -eq "$((before_bait + 1))"
# 只取 tag 本身（第一個空白前）而非整條啟動指令：自從啟動指令帶了 worker
# brief 的 initial prompt，指令中就有雙引號，而 tmux 存 pane_start_command
# 時會把它們跳脫成 \"（實測 3.7b），拿整條原文 grep -F 必然落空。
# 測例前提要驗的本來就只是「對照 pane 的指令中段含同一 tag」
bait_cmd="$(< "$TESTROOT/bait-cmd.txt")"
bait_tag="${bait_cmd%% *}"
assert "tag 比對：對照 pane 確實含同一 tag 字串（測例前提成立）" \
  bash -c "$(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null \
    list-panes -a -F '#{pane_start_command}' | grep -Fq -- $(printf '%q' "$bait_tag")"
# 清掉對照 pane
while read -r bp bcmd; do
  bcmd="${bcmd#\"}"   # tmux 存的是加了引號的整條指令，不剝就匹配不到
  [[ "$bcmd" == "sleep 600 # bait "* ]] && tmx kill-pane -t "$bp" 2>/dev/null
done < <(tmx list-panes -a -F '#{pane_id} #{pane_start_command}')

# 19c. 審計寫入失敗仍須完整回滾且釋放鎖（codex FAIL 3 的路徑）：
# agents.log 做成目錄 → log_agent_event 的 >> 失敗 → set -e 觸發 EXIT trap
DTRAP="$TESTROOT/dtrap"
mkdir -p "$DTRAP/agents" "$DTRAP/agents.log" "$DTRAP/locks" "$DTRAP/tasks"
before_trap="$(pane_count)"
absp "$DTRAP" 0 spawn audit-x --runtime codex >/dev/null 2>&1; rc=$?
assert "審計寫入失敗：非零退出" test "$rc" -ne 0
assert "審計寫入失敗：registry 鎖仍被釋放（無殘鎖）" \
  test ! -d "$DTRAP/locks/agents-registry.lock"
assert "審計寫入失敗：pane 被回收（pane 數不變）" \
  test "$(pane_count)" -eq "$before_trap"
assert "審計寫入失敗：不留 registry" test ! -e "$DTRAP/agents/audit-x.json"

# 19c'. 回滾刪不掉 registry 時不得靜默（codex 第二輪 FAIL 2）：
# date shim 在 registry 已寫入後把 agents/ 轉唯讀 → 審計失敗觸發回滾，
# 但 rm 也失敗。殘留 registry 會繼續佔 cap，必須明講而非吞掉。
DRES="$TESTROOT/dres"
mkdir -p "$DRES/agents" "$DRES/agents.log" "$DRES/locks" "$DRES/tasks"
RESSHIM="$TESTROOT/resshim"
mkdir -p "$RESSHIM"
cat > "$RESSHIM/date" <<EOF
#!/usr/bin/env bash
# registry 一旦落地就鎖住父目錄，讓後續回滾的 rm 必然失敗
if [[ -f "$DRES/agents/res-x.json" ]]; then chmod 0555 "$DRES/agents"; fi
exec /usr/bin/date "\$@"
EOF
chmod +x "$RESSHIM/date"
before_res="$(pane_count)"
env AGENT_BRIDGE_DATA="$DRES" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$RESSHIM:$SHIM:$PATH" \
  "$BRIDGE" spawn res-x --runtime codex >/dev/null 2>"$TESTROOT/res.err"; rc=$?
chmod 0755 "$DRES/agents"
assert "回滾刪不掉 registry：非零退出" test "$rc" -ne 0
assert "回滾刪不掉 registry：stderr 明確警告而非靜默" \
  grep -q '回滾未能刪除 registry' "$TESTROOT/res.err"
assert "回滾刪不掉 registry：仍釋放鎖（無殘鎖）" \
  test ! -d "$DRES/locks/agents-registry.lock"
assert "回滾刪不掉 registry：pane 仍被回收" \
  test "$(pane_count)" -eq "$before_res"

# 19d. 回滾殺不掉 pane 時也要出聲（codex 第三輪 FAIL 3）：
# kill-pane 固定失敗 + agents.log 為目錄觸發回滾 → 必須警告而非靜默
DRK="$TESTROOT/drk"
mkdir -p "$DRK/agents" "$DRK/agents.log" "$DRK/locks" "$DRK/tasks"
RKSHIM="$TESTROOT/rkshim"
mkdir -p "$RKSHIM"
cat > "$RKSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
# 回滾的 kill 現在包在 if-shell 內（原子驗 tag），攔它＝kill 未生效
if [[ "\$1" == "if-shell" ]]; then exit 0; fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$RKSHIM/tmux"
ln -s "$BRIDGE" "$RKSHIM/agent-bridge"
ln -s "$SHIM/codex" "$RKSHIM/codex"
env AGENT_BRIDGE_DATA="$DRK" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$RKSHIM:$PATH" \
  "$BRIDGE" spawn rk --runtime codex >/dev/null 2>"$TESTROOT/rk.err"; rc=$?
assert "回滾殺不掉 pane：非零退出" test "$rc" -ne 0
assert "回滾殺不掉 pane：stderr 明確警告而非靜默" \
  grep -q '回滾未能關閉 pane' "$TESTROOT/rk.err"
assert "回滾殺不掉 pane：仍釋放鎖" test ! -d "$DRK/locks/agents-registry.lock"
# 收拾這個刻意殺不掉的 pane
while read -r rp rcmd; do
  rcmd="${rcmd#\"}"
  [[ "$rcmd" == AGENT_BRIDGE_SPAWN_TAG=* ]] && tmx kill-pane -t "$rp" 2>/dev/null
done < <(tmx list-panes -a -F '#{pane_id} #{pane_start_command}')

# 19e. 回滾也要原子驗 tag（codex 第五輪 FAIL）：split-window 走 A server 拿到
# pane id，其餘呼叫落到 B server（同一個 id 在那裡是人工 pane）。回滾若直接
# kill 那個 id，就殺掉 B 的人工 pane；驗 tag 才殺則不動它。
SOCKC="agent-bridge-test-c"
"$REAL_TMUX" -L "$SOCKC" -f /dev/null new-session -d -s c -x 200 -y 100 "$pane_cmd"
mapfile -t CPANES < <("$REAL_TMUX" -L "$SOCKC" -f /dev/null list-panes -a -F '#{pane_id}')
PANE_C0="${CPANES[0]}"
DRBSWAP="$TESTROOT/drbswap"
mkdir -p "$DRBSWAP/agents" "$DRBSWAP/agents.log" "$DRBSWAP/locks" "$DRBSWAP/tasks"
RBSWAPSHIM="$TESTROOT/rbswapshim"
mkdir -p "$RBSWAPSHIM"
cat > "$RBSWAPSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
# 建 pane 走 A server，但回報 B server 上那個人工 pane 的 id（模擬 id 相同、
# 後續呼叫落到另一個 server）；其餘呼叫一律走 B server
if [[ "\$1" == "split-window" || "\$1" == "new-window" ]]; then
  $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@" >/dev/null
  printf '%s\n' $(printf '%q' "$PANE_C0")
  exit 0
fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCKC") -f /dev/null "\$@"
EOF
chmod +x "$RBSWAPSHIM/tmux"
ln -s "$BRIDGE" "$RBSWAPSHIM/agent-bridge"
ln -s "$SHIM/codex" "$RBSWAPSHIM/codex"
env AGENT_BRIDGE_DATA="$DRBSWAP" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$RBSWAPSHIM:$PATH" \
  "$BRIDGE" spawn rbsw --runtime codex >/dev/null 2>"$TESTROOT/rbswap.err"; rc=$?
assert "回滾跨 server：非零退出" test "$rc" -ne 0
assert "回滾跨 server：另一 server 的同 id 人工 pane 未被誤殺" \
  bash -c "$(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCKC") -f /dev/null \
    list-panes -a -F '#{pane_id}' | grep -Fxq $(printf '%q' "$PANE_C0")"
assert "回滾跨 server：stderr 有未能關閉 pane 的警告" \
  grep -q '回滾未能關閉 pane' "$TESTROOT/rbswap.err"
assert "回滾跨 server：不留 registry" test ! -e "$DRBSWAP/agents/rbsw.json"
assert "回滾跨 server：仍釋放鎖" test ! -d "$DRBSWAP/locks/agents-registry.lock"

# 回滾期間 tmux 整個不可用：不得把「查不到」當成「pane 已消失」而靜默
DRBNT="$TESTROOT/drbnt"
mkdir -p "$DRBNT/agents" "$DRBNT/agents.log" "$DRBNT/locks" "$DRBNT/tasks"
NTSHIM="$TESTROOT/ntshim"
mkdir -p "$NTSHIM"
cat > "$NTSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
# 只讓建 pane 成功，之後的查詢／if-shell 全部失敗（模擬 tmux 中途不可用）
if [[ "\$1" == "split-window" || "\$1" == "new-window" ]]; then
  exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
fi
if [[ "\$1" == "list-panes" && "\$*" != *pane_start_command* ]]; then
  echo "tmux: unavailable (test stub)" >&2; exit 1
fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$NTSHIM/tmux"
ln -s "$BRIDGE" "$NTSHIM/agent-bridge"
ln -s "$SHIM/codex" "$NTSHIM/codex"
env AGENT_BRIDGE_DATA="$DRBNT" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$NTSHIM:$PATH" \
  "$BRIDGE" spawn nt --runtime codex >/dev/null 2>"$TESTROOT/rbnt.err"; rc=$?
assert "回滾期間 tmux 查詢失敗：非零退出" test "$rc" -ne 0
assert "回滾期間 tmux 查詢失敗：警告而非靜默當成已回收" \
  grep -q '回滾未能關閉 pane' "$TESTROOT/rbnt.err"

# 同一個坑的另一半：pane id 遺失走 tag 掃描分支，而掃描用的 list-panes 也失敗
# → 迴圈跑零次，看起來跟「沒有孤兒」一樣。不出聲就是謊報回滾乾淨
DSCAN="$TESTROOT/dscan"
SCANSHIM="$TESTROOT/scanshim"
mkdir -p "$SCANSHIM"
cat > "$SCANSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
# 建 pane 成功但丟棄 -P 輸出（pane id 遺失 → 走 tag 掃描）
if [[ "\$1" == "split-window" || "\$1" == "new-window" ]]; then
  $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@" >/dev/null
  exit 0
fi
# 掃描用的查詢失敗
if [[ "\$1" == "list-panes" && "\$*" == *pane_start_command* ]]; then
  echo "tmux: unavailable (test stub)" >&2; exit 1
fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$SCANSHIM/tmux"
ln -s "$BRIDGE" "$SCANSHIM/agent-bridge"
ln -s "$SHIM/codex" "$SCANSHIM/codex"
env AGENT_BRIDGE_DATA="$DSCAN" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SCANSHIM:$PATH" \
  "$BRIDGE" spawn scan --runtime codex >/dev/null 2>"$TESTROOT/scan.err"; rc=$?
assert "孤兒掃描查詢失敗：非零退出" test "$rc" -ne 0
assert "孤兒掃描查詢失敗：警告而非靜默（不得謊報回滾乾淨）" \
  grep -q '回滾無法查詢 tmux pane' "$TESTROOT/scan.err"
assert "孤兒掃描查詢失敗：不留 registry" test ! -e "$DSCAN/agents/scan.json"
while read -r rp rcmd; do
  rcmd="${rcmd#\"}"
  [[ "$rcmd" == AGENT_BRIDGE_SPAWN_TAG=* ]] && tmx kill-pane -t "$rp" 2>/dev/null
done < <(tmx list-panes -a -F '#{pane_id} #{pane_start_command}')
while read -r rp rcmd; do
  rcmd="${rcmd#\"}"
  [[ "$rcmd" == AGENT_BRIDGE_SPAWN_TAG=* ]] && tmx kill-pane -t "$rp" 2>/dev/null
done < <(tmx list-panes -a -F '#{pane_id} #{pane_start_command}')
"$REAL_TMUX" -L "$SOCKC" -f /dev/null kill-server 2>/dev/null || true
# A server 上那個真的被建出來的 pane 要收掉
while read -r rp rcmd; do
  rcmd="${rcmd#\"}"
  [[ "$rcmd" == AGENT_BRIDGE_SPAWN_TAG=* ]] && tmx kill-pane -t "$rp" 2>/dev/null
done < <(tmx list-panes -a -F '#{pane_id} #{pane_start_command}')

# 目錄型 registry 路徑：名稱衝突檢查用 -e，不得把目錄當成「未註冊」而寫進去
DDIR="$TESTROOT/ddir"
mkdir -p "$DDIR/agents/dirname.json" "$DDIR/locks" "$DDIR/tasks"
before_ddir="$(pane_count)"
absp "$DDIR" 0 spawn dirname --runtime codex >/dev/null 2>&1; rc=$?
assert "目錄型 registry 路徑：spawn 被拒（非零退出）" test "$rc" -ne 0
assert "目錄型 registry 路徑：不建 pane" test "$(pane_count)" -eq "$before_ddir"
assert "目錄型 registry 路徑：無殘鎖" \
  test ! -d "$DDIR/locks/agents-registry.lock"

# ---- 20. 損壞 registry 的出身判斷必須 fail-closed（codex 第七輪 FAIL 1） ----
# `jq -e '.spawned == true'` 會把「條件 false」與「JSON 壞掉」壓成同一個非零，
# 於是損壞的 registry 被當成人工註冊：可被 register 覆寫、被 unregister 除名
# （pane 變孤兒）、也不計入 cap
DBAD="$TESTROOT/dbad"
pane_bad="$(absp "$DBAD" 0 spawn bad1 --runtime codex 2>/dev/null)"
printf '{' > "$DBAD/agents/bad1.json"     # 截斷成不合法 JSON
assert_fails "損壞 registry：unregister 拒絕（否則 pane 變孤兒）" \
  ab "$DBAD" unregister bad1
assert "損壞 registry：unregister 被拒後檔案仍在" test -f "$DBAD/agents/bad1.json"
assert "損壞 registry：pane 仍在（沒被除名成孤兒）" pane_alive "$pane_bad"
assert_fails "損壞 registry：register 拒絕覆寫" ab "$DBAD" register bad1 "$PANE_A"
assert_fails "損壞 registry：despawn 拒絕（出身不明不動 pane）" ab "$DBAD" despawn bad1
assert_fails "損壞 registry：ready 拒絕" ab "$DBAD" ready bad1
assert_fails "損壞 registry：disposable 拒絕（出身不明不得標記可回收）" \
  ab "$DBAD" disposable bad1
assert "損壞 registry：以上拒絕都不動 pane" pane_alive "$pane_bad"
# cap 保守計入：上限 1 且已有一份損壞 registry → 不得再 spawn
env AGENT_BRIDGE_DATA="$DBAD" AGENT_BRIDGE_MAX_SPAWN=1 AGENT_BRIDGE_READY_TIMEOUT=0 \
  PATH="$SHIM:$PATH" "$BRIDGE" spawn bad2 --runtime codex >/dev/null 2>&1; rc=$?
assert "損壞 registry：保守計入 cap（不得繞過上限）" test "$rc" -ne 0
assert "損壞 registry：cap 擋下後不留 registry" test ! -e "$DBAD/agents/bad2.json"

# 合法 JSON 但不是 object（字面 null）同樣判不出出身，不得被當成人工註冊
printf 'null' > "$DBAD/agents/bad1.json"
assert_fails "registry 是字面 null：unregister 拒絕" ab "$DBAD" unregister bad1
assert_fails "registry 是字面 null：register 拒絕覆寫" \
  ab "$DBAD" register bad1 "$PANE_A"
assert "registry 是字面 null：pane 未受影響" pane_alive "$pane_bad"

rm -f "$DBAD/agents/bad1.json"
tmx kill-pane -t "$pane_bad" 2>/dev/null

# ---- 20b. readiness 參數在建 pane 前就要驗（codex 第八輪 FAIL） ----
# 非法值若留到 spawn_wait_ready 才 die，pane 與 registry 都已落地、回滾也已
# 解除，呼叫端只看到非零退出，卻多了一個佔 cap 的 worker
DRO="$TESTROOT/dro"
mkdir -p "$DRO/agents" "$DRO/locks" "$DRO/tasks"
for badopt in "AGENT_BRIDGE_READY_TIMEOUT=abc" "AGENT_BRIDGE_READY_PROBE_INTERVAL=invalid" \
              "AGENT_BRIDGE_READY_PROBE_INTERVAL=0" "AGENT_BRIDGE_READY_PROBE_INTERVAL=.0" \
              "AGENT_BRIDGE_READY_PROBE_INTERVAL=.00" "AGENT_BRIDGE_READY_PROBE_INTERVAL=00"; do
  ro_before="$(pane_count)"
  env AGENT_BRIDGE_DATA="$DRO" "$badopt" PATH="$SHIM:$PATH" \
    "$BRIDGE" spawn "ro" --runtime codex >/dev/null 2>"$TESTROOT/ro.err"; rc=$?
  assert "readiness 參數不合法（$badopt）：非零退出" test "$rc" -ne 0
  assert "readiness 參數不合法（$badopt）：不建 pane" \
    test "$(pane_count)" -eq "$ro_before"
  assert "readiness 參數不合法（$badopt）：不留 registry" \
    test ! -e "$DRO/agents/ro.json"
done

# 註冊之後的失敗不得回報成 spawn 失敗：worker 已存在且佔著 cap，呼叫端照著
# 「失敗」重試或放棄都是錯的。stdout 被關（printf 失敗）是最實際的一種
DSO="$TESTROOT/dso"
env AGENT_BRIDGE_DATA="$DSO" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
  "$BRIDGE" spawn so --runtime codex >&- 2>/dev/null; rc=$?
assert "stdout 已關閉：spawn 仍 exit 0（worker 已建立不該報失敗）" test "$rc" -eq 0
assert "stdout 已關閉：registry 確實寫入" test -f "$DSO/agents/so.json"
ab "$DSO" despawn so >/dev/null 2>&1 || true

# ---- 21. 解鎖失敗不得靜默（codex 第七輪 FAIL 3） ----
# 鎖目錄被塞進檔案時 rmdir 會 Directory not empty，吞掉的話鎖就永久殘留、
# 而且沒有任何人知道
DLK="$TESTROOT/dlk"
mkdir -p "$DLK/agents" "$DLK/locks" "$DLK/tasks"
LKSHIM="$TESTROOT/lkshim"
mkdir -p "$LKSHIM"
cat > "$LKSHIM/date" <<EOF
#!/usr/bin/env bash
# spawn 取得 registry 鎖後才會呼叫 date（寫 registered_at）；趁機把鎖目錄弄成非空
if [[ -d "$DLK/locks/agents-registry.lock" ]]; then
  : > "$DLK/locks/agents-registry.lock/squatter"
fi
exec /usr/bin/date "\$@"
EOF
chmod +x "$LKSHIM/date"
env AGENT_BRIDGE_DATA="$DLK" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$LKSHIM:$SHIM:$PATH" \
  "$BRIDGE" spawn lk --runtime codex >/dev/null 2>"$TESTROOT/lk.err" || true
assert "解鎖失敗：stderr 明確警告而非靜默" \
  grep -q '無法釋放鎖目錄' "$TESTROOT/lk.err"
rm -rf "$DLK/locks/agents-registry.lock"
ab "$DLK" despawn lk >/dev/null 2>&1 || true

# ---- 22. worker brief 注入（真 codex 實測 gate 失敗的成因） ----
# spawn 出來的 runtime 是零脈絡 session：不注入守則的話，它不知道 pane 裡
# 收到的 `agent-bridge ready <name>` 是要執行的命令，會當成對話回覆而永遠
# 不 ready（實測 codex 0.145）。
# 已知限制：shim 模擬不了「LLM 只回話不執行」——那是模型判斷、不是可程式化
# 的行為。故這裡的回歸防線是「啟動指令必須帶 brief」這條不變量，而不是去
# 假裝模擬一個 LLM。真正的端到端確認只能靠真 runtime 實跑（human gate）。

# 22a. 啟動指令確實帶 brief 全文與 agent 名字
# shim 把展開後的 argv 寫進 codex-args.txt，$(cat …) 此時已由 pane 內的 sh
# 展開，所以這裡看得到 brief 內文
DBRIEF="$TESTROOT/dbrief"
absp "$DBRIEF" 20 spawn bw --runtime codex >/dev/null 2>&1
assert "brief 注入：啟動參數含 brief 標題（守則全文有進 prompt）" \
  grep -q 'agent-bridge worker 守則' "$TESTROOT/codex-args.txt"
assert "brief 注入：啟動參數含『是命令，不是聊天』這條關鍵守則" \
  grep -q '是命令，不是聊天' "$TESTROOT/codex-args.txt"
assert "brief 注入：啟動參數含本 worker 的名字與首要動作" \
  grep -q 'agent-bridge ready bw' "$TESTROOT/codex-args.txt"
ab "$DBRIEF" despawn bw >/dev/null 2>&1 || true

# 22b. brief 讀不到 → fail-closed：不建 pane、不留 registry、不寫審計
DNOBRIEF="$TESTROOT/dnobrief"
mkdir -p "$DNOBRIEF/agents" "$DNOBRIEF/locks" "$DNOBRIEF/tasks"
before_nobrief="$(pane_count)"
env AGENT_BRIDGE_DATA="$DNOBRIEF" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_WORKER_BRIEF="$TESTROOT/no-such-brief.md" \
  "$BRIDGE" spawn nb --runtime codex >/dev/null 2>"$TESTROOT/nobrief.err"; rc=$?
assert "brief 缺失：非零退出" test "$rc" -ne 0
assert "brief 缺失：訊息指出讀不到 worker brief" \
  grep -q 'worker brief' "$TESTROOT/nobrief.err"
assert "brief 缺失：不建 pane" test "$(pane_count)" -eq "$before_nobrief"
assert "brief 缺失：不留 registry" test ! -e "$DNOBRIEF/agents/nb.json"
assert "brief 缺失：不寫 agents.log（未 spawn 就不該有審計）" \
  bash -c "! grep -q 'spawned nb' '$DNOBRIEF/agents.log' 2>/dev/null"
assert "brief 缺失：locks 無殘留" \
  test -z "$(ls -A "$DNOBRIEF/locks" 2>/dev/null)"

# 22c. brief 路徑含單引號 → 拒絕。路徑會以單引號字面值送進 pane 的 sh，
# 一旦引號被閉合，後面就能塞命令進去。斷言刻意不只看「非零退出」：拿掉那道
# 檢查後，多數畸形路徑會讓 sh 引號不平衡而語法錯誤、被既有的夭折偵測擋下，
# 看起來照樣 fail-closed——測例會通過但什麼也沒鎖住（本測例第一版就是這樣，
# 破壞驗證才抓到）。故用一條「閉合引號後接命令、再開回引號」的路徑，
# 真正要斷言的是那個命令沒有被執行
DQUOTE="$TESTROOT/dquote"
mkdir -p "$DQUOTE/agents" "$DQUOTE/locks" "$DQUOTE/tasks"
PWNED="$TESTROOT/pwned-by-brief-path"
rm -f "$PWNED"
QBRIEF="$TESTROOT/x'; touch $PWNED; '.md"
# payload 內含絕對路徑 → 這個「檔名」帶斜線，實際上是多層路徑；父目錄不先
# 建出來，檔案就落不了地，spawn 會停在前一道 -r 檢查、測例變成在測「檔案
# 不存在」而不是單引號（第一版正是如此，破壞驗證才抓到）
mkdir -p "$(dirname "$QBRIEF")"
printf 'x\n' > "$QBRIEF"
before_quote="$(pane_count)"
env AGENT_BRIDGE_DATA="$DQUOTE" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_WORKER_BRIEF="$QBRIEF" \
  "$BRIDGE" spawn qb --runtime codex >/dev/null 2>"$TESTROOT/quote.err"; rc=$?
assert "brief 路徑含單引號：非零退出" test "$rc" -ne 0
assert "brief 路徑含單引號：訊息指出是單引號的問題" \
  grep -q '單引號' "$TESTROOT/quote.err"
assert "brief 路徑含單引號：路徑裡夾帶的命令未被執行（注入防線）" \
  test ! -e "$PWNED"
assert "brief 路徑含單引號：不建 pane" test "$(pane_count)" -eq "$before_quote"
assert "brief 路徑含單引號：不留 registry" test ! -e "$DQUOTE/agents/qb.json"

# 22d. brief 是目錄／非普通檔 → 拒絕（codex 複核 FAIL 1）
# `[[ -r ]]` 對目錄成立，而 pane 內 cat 讀目錄會失敗、命令替換卻仍回空字串，
# runtime 照樣被 exec 起來——fail-closed 承諾會變成開出一個沒有守則的 worker
DDIR="$TESTROOT/ddir"
mkdir -p "$DDIR/agents" "$DDIR/locks" "$DDIR/tasks" "$TESTROOT/brief-as-dir"
before_dir="$(pane_count)"
env AGENT_BRIDGE_DATA="$DDIR" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_WORKER_BRIEF="$TESTROOT/brief-as-dir" \
  "$BRIDGE" spawn db --runtime codex >/dev/null 2>"$TESTROOT/dir.err"; rc=$?
assert "brief 是目錄：非零退出" test "$rc" -ne 0
assert "brief 是目錄：訊息指出不是普通檔案" \
  grep -q '普通檔案' "$TESTROOT/dir.err"
assert "brief 是目錄：不建 pane" test "$(pane_count)" -eq "$before_dir"
assert "brief 是目錄：不留 registry" test ! -e "$DDIR/agents/db.json"

# 22e. brief 路徑以 `-` 開頭 → cat 必須用 -- 隔開（codex 複核 FAIL 2）
# AGENT_BRIDGE_WORKER_BRIEF 是公開介面、接得到相對路徑；少了 option
# terminator，一個叫 --help 的檔案會被 cat 當選項，spawn 成功卻注入錯 prompt
DDASH="$TESTROOT/ddash"
mkdir -p "$DDASH/agents" "$DDASH/locks" "$DDASH/tasks" "$TESTROOT/dashdir"
printf '# DASHBRIEF-SENTINEL 守則內容\n' > "$TESTROOT/dashdir/--help"
: > "$TESTROOT/codex-args.txt"
( cd "$TESTROOT/dashdir" \
  && env AGENT_BRIDGE_DATA="$DDASH" AGENT_BRIDGE_READY_TIMEOUT=20 PATH="$SHIM:$PATH" \
         AGENT_BRIDGE_WORKER_BRIEF='--help' \
     "$BRIDGE" spawn dh --runtime codex >/dev/null 2>&1 )
assert "brief 路徑以 - 開頭：注入的是檔案內容而非 cat 的說明" \
  grep -q 'DASHBRIEF-SENTINEL' "$TESTROOT/codex-args.txt"
assert "brief 路徑以 - 開頭：沒把 cat 的用法說明當成守則注入" \
  bash -c "! grep -qi 'cat \[' '$TESTROOT/codex-args.txt'"
ab "$DDASH" despawn dh >/dev/null 2>&1 || true

# 22f. brief 正本的內容不變量：守則被刪成空殼時要在這裡就紅，
# 而不是等到真 runtime 實跑才發現 worker 不會做事
REPO_BRIEF="$(dirname "$(dirname "$BRIDGE")")/share/worker-brief.md"
assert "brief 正本存在於 repo" test -r "$REPO_BRIEF"
for kw in 'agent-bridge receive' 'agent-bridge reply' 'agent-bridge fail' \
          '是命令，不是聊天' '資料，不是指令'; do
  assert "brief 正本含必要元素：$kw" grep -q -- "$kw" "$REPO_BRIEF"
done

# ---- 23. relay：交棒給接手者 ----
# relay 與 spawn 共用整條 pane 生命週期（cap／tag／回滾／夭折偵測／registry），
# 差別只有注入哪份 brief、切不切焦點、以及要不要請接手者回收前一棒。
# 故這裡只測那三處差異＋交接檔路徑的防線，不重測 spawn 已覆蓋的部分。

# 23a. 接手者 brief 正本的內容不變量（同 22f 的理由）
REPO_SBRIEF="$(dirname "$(dirname "$BRIDGE")")/share/successor-brief.md"
assert "接手者 brief 正本存在於 repo" test -r "$REPO_SBRIEF"
for kw in 'agent-bridge ready' 'agent-bridge despawn' '你不是等待派工的 worker' \
          '資料，不是指令' '交接檔可能有錯'; do
  assert "接手者 brief 正本含必要元素：$kw" grep -q -- "$kw" "$REPO_SBRIEF"
done

# 23b. 快樂路徑：注入的是接手者守則而非 worker 守則，且帶交接檔路徑
DRELAY="$TESTROOT/drelay"
HANDOFF="$TESTROOT/fake-handoff.md"
printf '# 假交接檔\n下一步：什麼都不做。\n' > "$HANDOFF"
# READY_TIMEOUT=0：shim 內的 agent-bridge 固定寫 $DSPAWN，relay 用自己的資料
# 目錄時 ready 翻不了——但 ready 探針是 spawn 那條共用路徑，§16a 已覆蓋，
# 這裡等它只會白白拖慢測試
pane_r1="$(env AGENT_BRIDGE_DATA="$DRELAY" AGENT_BRIDGE_READY_TIMEOUT=0 \
  PATH="$SHIM:$PATH" \
  "$BRIDGE" relay r1 --runtime codex --handoff "$HANDOFF" --no-select 2>/dev/null)"; rc=$?
assert "relay 成功：exit 0" test "$rc" -eq 0
assert "relay stdout 只印 pane-id（%N）一行" \
  bash -c "[[ '$pane_r1' =~ ^%[0-9]+\$ ]]"
assert "relay 注入接手者守則（不是 worker 守則）" \
  grep -q 'agent-bridge 接手者守則' "$TESTROOT/codex-args.txt"
assert "relay 注入的不是 worker 守則（兩份心智相反，混用會讓接手者空等 receive）" \
  bash -c "! grep -q 'agent-bridge worker 守則' '$TESTROOT/codex-args.txt'"
assert "relay 啟動參數含交接檔路徑" \
  grep -qF -- "$HANDOFF" "$TESTROOT/codex-args.txt"
assert "relay 啟動參數含本接手者的名字與首要動作" \
  grep -q 'agent-bridge ready r1' "$TESTROOT/codex-args.txt"
assert "relay registry：與 spawn 同一套欄位（共用 cmd_spawn）" \
  jq -e '.spawned == true and .runtime == "codex" and (.spawned_at | type == "string")' \
  "$DRELAY/agents/r1.json"
assert "relay 寫 agents.log（審計線不因換命令而斷）" \
  grep -qE "Z spawned r1 ${pane_r1} codex\$" "$DRELAY/agents.log"

# 23b2. relay --model 直通 cmd_spawn（驗證正本在 spawn 的解析點，relay 不抄第二份）
env AGENT_BRIDGE_DATA="$DRELAY" AGENT_BRIDGE_READY_TIMEOUT=0 \
  PATH="$SHIM:$PATH" \
  "$BRIDGE" relay r1m --runtime codex --model gpt-relay --handoff "$HANDOFF" --no-select >/dev/null 2>&1; rc=$?
assert "relay --model：exit 0" test "$rc" -eq 0
assert "relay 啟動參數含相鄰的 --model gpt-relay" \
  grep -q -- '--model gpt-relay' "$TESTROOT/codex-args.txt"
assert "relay --model 進 registry" \
  jq -e '.model == "gpt-relay"' "$DRELAY/agents/r1m.json"
assert_fails "relay 的不合法 model 一樣被 spawn 解析點擋下" \
  env AGENT_BRIDGE_DATA="$DRELAY" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
  "$BRIDGE" relay r1x --runtime codex --model 'x;kill-server' --handoff "$HANDOFF" --no-select
assert "被拒的 relay 不留 registry" \
  bash -c "[[ ! -e '$DRELAY/agents/r1x.json' ]]"
assert "relay --model 的接手者可正常 despawn（不佔用後續 cap）" \
  env AGENT_BRIDGE_DATA="$DRELAY" PATH="$SHIM:$PATH" "$BRIDGE" despawn r1m
# 未指定 --self-exit 時不該憑空冒出回收指示。
# 不能拿 'agent-bridge despawn' 當關鍵字——接手者 brief 內文本來就有那一段
# （說明被拒是正常的），會恆綠。要鎖的是動態尾巴本身
assert "relay 未指定 --self-exit：prompt 不含回收前一棒的尾巴" \
  bash -c "! grep -q '接手完成後，回收前一棒' '$TESTROOT/codex-args.txt'"
ab "$DRELAY" despawn r1 >/dev/null 2>&1 || true

# 23c. --self-exit：把回收前一棒的指示寫進接手者的 prompt。
# 注意這裡鎖的是「指示有送到」，不是「A 真的被殺」——動手的是接手者，
# 那條路徑就是既有的 cmd_despawn（已由 §17 等覆蓋）
env AGENT_BRIDGE_DATA="$DRELAY" AGENT_BRIDGE_READY_TIMEOUT=20 \
  AGENT_BRIDGE_READY_PROBE_INTERVAL=0.5 PATH="$SHIM:$PATH" \
  "$BRIDGE" relay r2 --runtime codex --handoff "$HANDOFF" --no-select \
  --self-exit prev-agent >/dev/null 2>&1; rc=$?
assert "relay --self-exit：exit 0" test "$rc" -eq 0
assert "relay --self-exit：prompt 指示接手者 despawn 前一棒" \
  grep -q '接手完成後，回收前一棒：執行 agent-bridge despawn prev-agent' \
  "$TESTROOT/codex-args.txt"
assert "relay --self-exit：prompt 說明人工 pane 被拒是正常的（防接手者硬繞）" \
  grep -q '會被拒絕' "$TESTROOT/codex-args.txt"
ab "$DRELAY" despawn r2 >/dev/null 2>&1 || true

# 23d. --self-exit 的名稱會進 prompt 字面值，必須擋住不合法名稱
before_se="$(pane_count)"
env AGENT_BRIDGE_DATA="$DRELAY" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
  "$BRIDGE" relay r3 --runtime codex --handoff "$HANDOFF" \
  --self-exit 'bad;name' >/dev/null 2>"$TESTROOT/se.err"; rc=$?
assert "relay --self-exit 名稱不合法：非零退出" test "$rc" -ne 0
assert "relay --self-exit 名稱不合法：訊息指出名稱問題" \
  grep -q '名稱不合法' "$TESTROOT/se.err"
assert "relay --self-exit 名稱不合法：不建 pane" test "$(pane_count)" -eq "$before_se"
assert "relay --self-exit 名稱不合法：不留 registry" test ! -e "$DRELAY/agents/r3.json"

# 23e. 交接檔路徑的防線：不存在／是目錄／含單引號。
# 三道都必須在建 pane 之前擋下——pane 落地後才 die 會留下佔 cap 的孤兒。
#
# 關於「夾帶的命令未被執行」那條斷言的實測結果（破壞驗證 M6，值得記下來）：
# 把 cmd_relay 與 relay_prompt_arg 兩道單引號檢查都拆掉後，紅的是「非零退出」
# 「訊息指出單引號」「不建 pane」三條，**注入斷言本身沒紅**。另以獨立小腳本
# 確認：同一個 payload 直接餵給 `sh -c` 時 touch 確實會執行（注入得手），
# 所以純 shell 層的危險是真的；但經過 tmux split-window 這一層之後它沒有落地，
# 原因未查明。故這條斷言在此路徑上只當「真的被注入時的最後保險」，
# **實際鎖住防線的是前三條**——別把它當成注入防線的主要證據。
before_ho="$(pane_count)"
env AGENT_BRIDGE_DATA="$DRELAY" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
  "$BRIDGE" relay r4 --runtime codex --handoff "$TESTROOT/no-such-handoff.md" \
  >/dev/null 2>"$TESTROOT/ho1.err"; rc=$?
assert "交接檔不存在：非零退出" test "$rc" -ne 0
assert "交接檔不存在：訊息指出交接檔問題" grep -q '交接檔' "$TESTROOT/ho1.err"
assert "交接檔不存在：不建 pane" test "$(pane_count)" -eq "$before_ho"
assert "交接檔不存在：不留 registry" test ! -e "$DRELAY/agents/r4.json"

env AGENT_BRIDGE_DATA="$DRELAY" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
  "$BRIDGE" relay r5 --runtime codex --handoff "$TESTROOT" \
  >/dev/null 2>"$TESTROOT/ho2.err"; rc=$?
assert "交接檔是目錄：非零退出" test "$rc" -ne 0
assert "交接檔是目錄：訊息指出不是普通檔案" \
  grep -q '不是可讀的普通檔案' "$TESTROOT/ho2.err"
assert "交接檔是目錄：不建 pane" test "$(pane_count)" -eq "$before_ho"

HO_MARK="$TESTROOT/relay-injected.txt"
rm -f "$HO_MARK"
# 這個惡意路徑必須真的存在，否則前一道 -f 檢查就把它擋下來，測例會變成在測
# 「檔案不存在」而不是單引號防線——拿掉單引號檢查也照樣綠（22c 踩過同一個坑）。
# payload 含絕對路徑 → 「檔名」帶斜線、實為多層路徑，父目錄要先建
HO_INJ="$TESTROOT/x'; touch $HO_MARK; '.md"
mkdir -p "$(dirname "$HO_INJ")"
printf 'x\n' > "$HO_INJ"
env AGENT_BRIDGE_DATA="$DRELAY" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
  "$BRIDGE" relay r6 --runtime codex \
  --handoff "$HO_INJ" >/dev/null 2>"$TESTROOT/ho3.err"; rc=$?
assert "交接檔路徑含單引號：非零退出" test "$rc" -ne 0
assert "交接檔路徑含單引號：訊息指出是單引號的問題" \
  grep -q '單引號' "$TESTROOT/ho3.err"
assert "交接檔路徑含單引號：路徑裡夾帶的命令未被執行（注入防線）" \
  test ! -e "$HO_MARK"
assert "交接檔路徑含單引號：不建 pane" test "$(pane_count)" -eq "$before_ho"

# 23f. 參數缺漏
assert_fails "relay 缺 --handoff 被拒" \
  env AGENT_BRIDGE_DATA="$DRELAY" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
  "$BRIDGE" relay r7 --runtime codex
assert "relay 缺 --handoff：不留 registry" test ! -e "$DRELAY/agents/r7.json"

# 23g. 接手者 brief 讀不到 → fail-closed（同 22b，但走 relay 這條路徑）
env AGENT_BRIDGE_DATA="$DRELAY" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_SUCCESSOR_BRIEF="$TESTROOT/no-such-sbrief.md" \
  "$BRIDGE" relay r8 --runtime codex --handoff "$HANDOFF" \
  >/dev/null 2>"$TESTROOT/sb.err"; rc=$?
assert "接手者 brief 缺失：非零退出" test "$rc" -ne 0
assert "接手者 brief 缺失：訊息指出讀不到接手者 brief" \
  grep -q '接手者 brief' "$TESTROOT/sb.err"
assert "接手者 brief 缺失：不建 pane" test "$(pane_count)" -eq "$before_ho"
assert "接手者 brief 缺失：不留 registry" test ! -e "$DRELAY/agents/r8.json"

# ---- 24. disposable：worker 自報脈絡無殘值（orchestrator Phase 1） ----
# 語意刻意是單向宣告而非雙向旗標：預設保留，沒宣告過的一律視為仍有殘值。
# 失效方向因此是「多留一個佔 cap」而不是「把還有用的脈絡殺掉」
DDISP="$TESTROOT/ddisp"
pane_dp1="$(absp "$DDISP" 0 spawn dp1 --runtime codex 2>/dev/null)"
assert "disposable 預設不存在：剛 spawn 的 registry 沒有這個欄位（預設＝保留）" \
  test "$(jq -r '.disposable // "absent"' "$DDISP/agents/dp1.json")" = absent
ab "$DDISP" disposable dp1 2>/dev/null; rc=$?
assert "disposable：exit 0" test "$rc" -eq 0
assert "disposable：registry 翻 true" \
  jq -e '.disposable == true' "$DDISP/agents/dp1.json"
assert "disposable：寫下 disposable_at 時間戳（宣告可過期的依據）" \
  jq -e '.disposable_at | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")' \
  "$DDISP/agents/dp1.json"
assert "disposable：不動 pane（只是宣告，不是回收）" pane_alive "$pane_dp1"
assert "disposable：寫 agents.log 審計線（回收爭議的還原依據）" \
  grep -qE "Z disposable dp1 " "$DDISP/agents.log"
# 這欄位是建議不是保護：回收認的仍是 spawn_tag，故宣告不得動到 tag
assert "disposable：spawn_tag 未被更動（回收仍認 tag，不認本欄位）" \
  jq -e '.spawn_tag | startswith("AGENT_BRIDGE_SPAWN_TAG=")' "$DDISP/agents/dp1.json"
assert "disposable：ready 狀態未被波及" \
  jq -e 'has("ready")' "$DDISP/agents/dp1.json"
ab "$DDISP" disposable dp1 2>/dev/null; rc=$?
assert "disposable：重複宣告仍 exit 0（冪等）" test "$rc" -eq 0

# 出身與輸入防護：形狀比照 ready
ab "$DDISP" register manual-d "$PANE_A" >/dev/null 2>&1
assert_fails "disposable：人工 agent 被拒（人工 pane 生命週期不歸 bridge 管）" \
  ab "$DDISP" disposable manual-d
assert "disposable：人工 agent 被拒後未寫入欄位" \
  test "$(jq -r '.disposable // "absent"' "$DDISP/agents/manual-d.json")" = absent
assert_fails "disposable：未註冊 agent 報錯" ab "$DDISP" disposable ghost
assert_fails "disposable：名稱不合法被拒" ab "$DDISP" disposable "bad name"
assert_fails "disposable：缺參數被拒" ab "$DDISP" disposable

# is_spawned 的「無法判定」分支（rc=2）必須能被單獨證明。§20 那條走的是截斷成
# 不合法 JSON 的路徑——那裡 jq 自己就會失敗、被 set -e 擋下，於是即使拆掉出身
# 檢查測例照樣綠＝空綠（本 repo 第五次踩到同一個坑）。用「合法 JSON 但不是
# object」才隔離得出 rc=2：jq 讀得動（null 取 field 回 null，`// "-"` 有預設值），
# 只有 is_spawned 會擋
printf 'null\n' > "$DDISP/agents/dp1.json"
assert_fails "disposable：registry 是合法 JSON 但非 object → 拒絕（出身不明）" \
  ab "$DDISP" disposable dp1
assert "disposable：非 object registry 被拒後內容未被覆寫" \
  test "$(tr -d '[:space:]' < "$DDISP/agents/dp1.json")" = null

# ---- 25. idle：worker 池回收決策視圖（orchestrator Phase 2） ----
DIDLE="$TESTROOT/didle"

# idle_field <data-dir> <agent> <欄位序號 1-4>
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
idle_field() {
  ab "$1" idle 2>/dev/null | awk -F'\t' -v n="$2" -v c="$3" '$1 == n {print $c; exit}'
}
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
is_num() { [[ "$1" =~ ^[0-9]+$ ]]; }

absp "$DIDLE" 0 spawn id1 --runtime codex >/dev/null 2>&1
assert "idle：tasks 目錄空時不崩（exit 0）" ab "$DIDLE" idle
assert "idle：輸出四欄 TSV（欄位順序是對外契約，orchestrator 依它決策）" \
  test "$(ab "$DIDLE" idle 2>/dev/null | awk -F'\t' 'NR==1{print NF}')" = 4
assert "idle：未就緒 spawned agent → ready 欄 starting" \
  test "$(idle_field "$DIDLE" id1 2)" = starting
assert "idle：未宣告 → disposable 欄為 -（預設保留）" \
  test "$(idle_field "$DIDLE" id1 3)" = "-"
assert "idle：從未被派工過也算得出 idle_secs（退回 registry 登記時間）" \
  is_num "$(idle_field "$DIDLE" id1 4)"

ab "$DIDLE" ready id1 >/dev/null 2>&1
assert "idle：ready 後欄位變 ready" test "$(idle_field "$DIDLE" id1 2)" = ready
ab "$DIDLE" disposable id1 >/dev/null 2>&1
assert "idle：宣告後 disposable 欄為 yes（可即時回收）" \
  test "$(idle_field "$DIDLE" id1 3)" = yes

# 宣告過期：這是 disposable_at 存在的唯一理由——worker 宣告後若又被派新任務，
# 它已累積新脈絡，舊宣告不該再讓 orchestrator 覺得可以直接收。
# 先測「明確晚於宣告」這個一般情形；同秒的邊界另有專門測例（見下）
sleep 1
ab "$DIDLE" send id1 --from alice --message "後續任務" >/dev/null 2>&1
assert "idle：宣告後又被派工 → disposable 轉 expired（宣告失效，不得誤收）" \
  test "$(idle_field "$DIDLE" id1 3)" = expired
assert "idle：派工後 idle_secs 改以最後任務時間計" \
  is_num "$(idle_field "$DIDLE" id1 4)"

# 同秒邊界：宣告與新任務落在同一秒（worker 宣告完 orchestrator 立刻派工）。
# 舊實作用嚴格大於，那一秒內 idle 仍報 yes，orchestrator 會直接 despawn 一個
# 剛拿到新任務、正在累積新脈絡的 worker——不走 evict、不留筆記，語意基石反轉。
# 這條原本被上面那個 sleep 1 繞過去了：測試迴避了缺陷而不是暴露它。
# （獨立複核 2026-07-22 以重現實驗抓出）
DSEC="$TESTROOT/dsamesec"
mkdir -p "$DSEC"/{agents,tasks,locks}
now_ts="$(date -u +%FT%TZ)"
jq -n --arg ts "$now_ts" \
  '{name:"samesec",pane_id:"%99",registered_at:$ts,spawned:true,runtime:"codex",
    spawned_at:$ts,ready:true,disposable:true,disposable_at:$ts}' \
  > "$DSEC/agents/samesec.json"
mkdir -p "$DSEC/tasks/${now_ts//[:-]/}-aaaa"
jq -n --arg ts "$now_ts" \
  '{id:"x",from:"alice",to:"samesec",created_at:$ts,status:"queued"}' \
  > "$DSEC/tasks/${now_ts//[:-]/}-aaaa/metadata.json"
assert "idle：宣告與新任務同秒 → 仍須轉 expired（否則會誤殺已有新脈絡的 worker）" \
  test "$(idle_field "$DSEC" samesec 3)" = expired

# 名稱重用：tasks/ 是長期累積的（GC 仍是 backlog），同一個名字可能有前一個
# pane 留下的任務。idle_secs 若採信那個時間，剛 spawn 的 worker 會顯示成閒置
# 好幾小時，LRU 就會優先驅逐一個還沒做過事的新 pane——決策視圖說謊。
# 真鏈驗收 2026-07-22 實地踩到：w1 才 spawn 39 秒卻顯示 21686。
ab "$DIDLE" send reborn --from alice --message "前世任務" >/dev/null 2>&1
old_dirs=( "$DIDLE"/tasks/*/ )   # 目錄名是 UTC 時間戳，glob 的字典序＝時間序
old_t="${old_dirs[-1]}"
printf '{"id":"x","from":"alice","to":"reborn","created_at":"2020-01-01T00:00:00Z"}\n' \
  > "$old_t/metadata.json"
absp "$DIDLE" 0 spawn reborn --runtime codex >/dev/null 2>&1
assert "idle：同名前一輪的舊任務不得讓新 pane 看起來閒置多年（LRU 會誤殺）" \
  test "$(idle_field "$DIDLE" reborn 4)" -lt 3600
ab "$DIDLE" despawn reborn >/dev/null 2>&1

# 人工註冊：生命週期不歸 bridge 管，兩欄都必須是 -
ab "$DIDLE" register manual-i "$PANE_A" >/dev/null 2>&1
assert "idle：人工 agent ready 欄為 -" test "$(idle_field "$DIDLE" manual-i 2)" = "-"
assert "idle：人工 agent disposable 欄為 -（不歸 bridge 回收）" \
  test "$(idle_field "$DIDLE" manual-i 3)" = "-"
# 即使被手動塞了 disposable 欄位，人工 agent 也不得顯示成可回收
printf '{"name":"manual-i","pane_id":"%s","registered_at":"2026-01-01T00:00:00Z","disposable":true,"disposable_at":"2026-01-01T00:00:00Z"}\n' \
  "$PANE_A" > "$DIDLE/agents/manual-i.json"
assert "idle：人工 agent 被手塞 disposable 仍顯示 -（不因可寫 registry 而可回收）" \
  test "$(idle_field "$DIDLE" manual-i 3)" = "-"

# 損壞 registry：不能讓整份報表消失，也不能靜靜跳過——它照樣佔著 cap
printf '{' > "$DIDLE/agents/broken.json"
assert "idle：損壞 registry 不讓整份報表掛掉（exit 0）" ab "$DIDLE" idle
assert "idle：損壞 registry 以 ? 標出（照樣佔 cap，不得靜靜跳過）" \
  test "$(idle_field "$DIDLE" broken 2)" = "?"
assert "idle：損壞 registry 不影響其他 agent 照常列出" \
  test "$(idle_field "$DIDLE" id1 2)" = ready

# 唯讀：orchestrator 可能跑在只讀 sandbox；查詢命令不該需要寫權限
snap_before="$(find "$DIDLE" | sort)"
ab "$DIDLE" idle >/dev/null 2>&1
snap_after="$(find "$DIDLE" | sort)"
assert "idle：唯讀——執行前後檔案清單完全一致（不寫任何檔）" \
  test "$snap_before" = "$snap_after"
assert "idle：不取鎖——執行後 locks/ 為空" \
  test -z "$(ls -A "$DIDLE/locks")"
assert_fails "idle：多給參數被拒" ab "$DIDLE" idle extra

# ---- 26. evict：驅逐前強制落地筆記（orchestrator Phase 3） ----
# 三步 send → await → despawn。核心不變量：**pane 沒被收掉之前，筆記一定先
# 落地**；唯一的例外是逾時，而逾時必須在審計線上看得出來（否則「筆記沒落地」
# 這件事會混進正常回收裡，事後查不出來）
DEV="$TESTROOT/devict"

# resp_has <data-dir> <task-id> <pattern>：收尾筆記的內容比對
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
resp_has() { ab "$1" read "$2" 2>/dev/null | grep -q "$3"; }

# task_count <data-dir>：tasks/ 底下的任務數（用來證明「被拒時不留孤兒任務」）
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
task_count() {
  local x n=0
  for x in "$1"/tasks/*/; do
    [[ -d "$x" ]] && n=$((n + 1))
  done
  printf '%s' "$n"
}

# bg_reply <data-dir> <agent> <message>：背景等該 agent 的新任務出現後 receive+reply。
# 前置 sleep 是刻意的：evict 若沒有「等筆記落地」那一步，它會在這個 replier
# 動手之前就 despawn 完畢，下面「evict 返回當下筆記已落地」的斷言才必定見紅。
# 沒有這段延遲，破壞-復原會變成時序賭博（可能空綠）
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
bg_reply() {
  local data="$1" who="$2" msg="$3"
  (
    sleep 0.8
    local i d tid=""
    for (( i = 0; i < 150; i++ )); do
      for d in "$data"/tasks/*/; do
        [[ -f "$d/metadata.json" ]] || continue
        [[ "$(jq -r '.to // ""' "$d/metadata.json" 2>/dev/null)" == "$who" ]] || continue
        [[ "$(<"$d/status")" == queued ]] || continue
        tid="$(basename "$d")"
        break
      done
      [[ -n "$tid" ]] && break
      sleep 0.2
    done
    [[ -n "$tid" ]] || exit 1
    ab "$data" receive "$tid" >/dev/null 2>&1
    ab "$data" reply "$tid" --message "$msg" >/dev/null 2>&1
  ) &
}

# 26a. 輸入與出身防護：一律在送出收尾任務**之前**擋掉。放行後才發現不該收，
# 會留下一個沒人回收的孤兒任務，而且 pane 白白被打擾一次
assert_fails "evict：缺參數被拒" ab "$DEV" evict
assert_fails "evict：未註冊 agent 報錯" ab "$DEV" evict ghost
assert_fails "evict：名稱不合法被拒" ab "$DEV" evict "bad name"
before_t="$(task_count "$DEV")"
ab "$DEV" register manual-e "$PANE_A" >/dev/null 2>&1
assert_fails "evict：人工 agent 被拒（生命週期不歸 bridge 管）" ab "$DEV" evict manual-e
assert "evict：人工 agent 被拒時不留孤兒收尾任務（擋在 send 之前）" \
  test "$(task_count "$DEV")" -eq "$before_t"
assert "evict：人工 agent 被拒後 registry 仍在（未被誤除名）" \
  test -e "$DEV/agents/manual-e.json"

pane_ev1="$(absp "$DEV" 0 spawn ev1 --runtime codex 2>/dev/null)"
assert_fails "evict：--timeout 非數字被拒" ab "$DEV" evict ev1 --timeout abc
assert_fails "evict：--timeout 缺參數被拒" ab "$DEV" evict ev1 --timeout
assert_fails "evict：--from 名稱不合法被拒" ab "$DEV" evict ev1 --from "bad name"
assert_fails "evict：未知參數被拒" ab "$DEV" evict ev1 --bogus
assert "evict：參數被拒時不留孤兒收尾任務" test "$(task_count "$DEV")" -eq "$before_t"
assert "evict：參數被拒時 pane 未被動" pane_alive "$pane_ev1"

# 26b. 成功路徑：收尾任務送出 → worker 回筆記 → pane 才被收
bg_reply "$DEV" ev1 "只存在我 context 裡的事實：X 在 foo.sh:42"
tid_ev1="$(ab "$DEV" evict ev1 --from alice 2>"$TESTROOT/ev1.err")"; rc=$?
# ★ 狀態要在 wait **之前**取樣。先 wait 等於等背景 replier 做完才看——那時
# 任務當然是 completed，即使 evict 根本沒等過。M1 實測確認：這個順序寫反，
# 「拆掉等待落地」的破壞-復原會整組空綠（本 repo 第六次踩同一個坑）
st_ev1_now="$(ab "$DEV" status "$tid_ev1" 2>/dev/null)"
wait
assert "evict：成功路徑 exit 0" test "$rc" -eq 0
assert "evict：stdout 只印一行收尾 task-id" \
  test "$(printf '%s\n' "$tid_ev1" | wc -l)" -eq 1
assert "evict：印出的 task-id 是真的存在的任務" test -d "$DEV/tasks/$tid_ev1"
assert "evict：收尾任務確實派給被驅逐者" \
  test "$(jq -r '.to' "$DEV/tasks/$tid_ev1/metadata.json")" = ev1
assert "evict：--from 進了任務 metadata（審計得出是誰發起驅逐）" \
  test "$(jq -r '.from' "$DEV/tasks/$tid_ev1/metadata.json")" = alice
# 收尾文案是機制的一部分：任務內容若是空的，worker 不會知道要寫什麼
assert "evict：收尾任務帶著收尾文案（不是一個空任務）" \
  grep -q '收尾任務' "$DEV/tasks/$tid_ev1/request.md"
# ★ 核心斷言：evict 返回的當下筆記就已經落地——不再等、不再輪詢。
# 拆掉 await 那一步的話，這裡會抓到 queued/delivered → 紅
assert "evict：返回當下收尾任務已 completed（筆記先落地才收 pane）" \
  test "$st_ev1_now" = completed
assert "evict：收尾筆記讀得回內容（read 拿得到 worker 寫的事實）" \
  resp_has "$DEV" "$tid_ev1" 'foo.sh:42'
assert "evict：registry 已除名" test ! -e "$DEV/agents/ev1.json"
assert_fails "evict：pane 已回收（pane_alive 不再成立）" pane_alive "$pane_ev1"
# 審計線：evicted 與 evicted-timeout 必須分得開，否則「筆記沒落地」查不出來
assert "evict：agents.log 記 evicted（筆記已落地的驅逐）" \
  grep -qE "Z evicted ev1 " "$DEV/agents.log"
assert_fails "evict：成功路徑不得記成 evicted-timeout" \
  grep -qE "Z evicted-timeout ev1 " "$DEV/agents.log"
assert "evict：成功路徑後 locks/ 為空（三步分段取鎖，不得有殘留）" \
  test -z "$(ls -A "$DEV/locks")"

# 26c. 逾時路徑：worker 不回話。**仍然要 despawn**——否則一個不回話的 worker
# 會把 cap 永久卡死，驅逐機制整個失效。代價是筆記沒落地，靠審計記號讓它可見
pane_ev2="$(absp "$DEV" 0 spawn ev2 --runtime codex 2>/dev/null)"
tid_ev2="$(ab "$DEV" evict ev2 --timeout 1 2>"$TESTROOT/ev2.err")"; rc=$?
assert "evict：逾時仍 exit 0（cap 確實騰出來了）" test "$rc" -eq 0
assert "evict：逾時仍印出收尾 task-id（事後仍可追這筆任務）" \
  test -d "$DEV/tasks/$tid_ev2"
assert "evict：逾時仍 despawn registry（不讓不回話的 worker 卡死 cap）" \
  test ! -e "$DEV/agents/ev2.json"
assert_fails "evict：逾時仍回收 pane（pane_alive 不再成立）" pane_alive "$pane_ev2"
assert "evict：逾時在 agents.log 留 evicted-timeout（筆記沒落地要看得見）" \
  grep -qE "Z evicted-timeout ev2 " "$DEV/agents.log"
assert_fails "evict：逾時不得記成正常 evicted" \
  grep -qE "Z evicted ev2 " "$DEV/agents.log"
assert "evict：逾時有 stderr 警告（呼叫端不會以為筆記拿到了）" \
  grep -q '逾時' "$TESTROOT/ev2.err"
assert "evict：逾時後收尾任務仍留在非終態（事後可查、可 cancel）" \
  test "$(ab "$DEV" status "$tid_ev2" 2>/dev/null)" != completed
assert "evict：逾時後 locks/ 為空" test -z "$(ls -A "$DEV/locks")"

# 26d. 未就緒（starting）的 worker 一樣收得掉：它可能就是卡在啟動才需要被驅逐
pane_ev3="$(absp "$DEV" 0 spawn ev3 --runtime codex 2>/dev/null)"
ab "$DEV" evict ev3 --timeout 1 >/dev/null 2>&1; rc=$?
assert "evict：未 ready 的 worker 也驅逐得掉（卡在啟動正是要收的情形）" \
  test "$rc" -eq 0
assert "evict：未 ready 的 worker 驅逐後 registry 已除名" \
  test ! -e "$DEV/agents/ev3.json"
assert_fails "evict：未 ready 的 worker 驅逐後 pane 已回收（pane_alive 不再成立）" \
  pane_alive "$pane_ev3"

# 26f. generation 綁定：evict 是三段式的，await 與 despawn 之間同名 agent 可能
# 已被換掉一代（另一次 evict、或 despawn＋重 spawn）。只憑名字 despawn 會殺掉
# 那個沒收過收尾任務、也沒宣告 disposable 的新 worker——這一層唯一不可接受的
# 失效。這裡用「回覆前先把 registry 的 spawn_tag 換掉」模擬換代。
# （獨立複核 2026-07-22 以逐行可達路徑指出，此測例補上動態重現）
# 換代用「真的 despawn 再 spawn 同名」而不是竄改 registry 的 tag：後者會讓
# registry 與 pane 啟動指令不一致，被既有的 despawn-stale 防線攔下，測不到最壞
# 情況。真實換代兩者是一致的（都屬 G2），stale 防線不會作用——只有 generation
# 綁定擋得住。換代做在回覆之前，讓時序可控：await 一返回，現場就已經是 G2。
pane_ev6="$(absp "$DEV" 0 spawn ev6 --runtime codex 2>/dev/null)"
gen2_file="$TESTROOT/ev6-gen2.pane"
(
  sleep 0.8
  for (( i = 0; i < 150; i++ )); do
    for d in "$DEV"/tasks/*/; do
      [[ -f "$d/metadata.json" ]] || continue
      [[ "$(jq -r '.to // ""' "$d/metadata.json" 2>/dev/null)" == ev6 ]] || continue
      [[ "$(<"$d/status")" == queued ]] || continue
      ab "$DEV" despawn ev6 >/dev/null 2>&1
      absp "$DEV" 0 spawn ev6 --runtime codex 2>/dev/null > "$gen2_file"
      ab "$DEV" receive "$(basename "$d")" >/dev/null 2>&1
      ab "$DEV" reply "$(basename "$d")" --message "筆記" >/dev/null 2>&1
      exit 0
    done
    sleep 0.2
  done
) &
ab "$DEV" evict ev6 --timeout 20 >/dev/null 2>&1; rc=$?
wait
pane_ev6_gen2="$(<"$gen2_file")"
assert "evict：目標在等待期間被換代 → 非零退出（不確定收的是誰就不收）" \
  test "$rc" -ne 0
assert "evict：換代後新一代的 registry 保留（那是別人的 worker，不得除名）" \
  test -e "$DEV/agents/ev6.json"
# ★ 核心斷言：G2 沒收過收尾任務、也沒宣告 disposable，它的 context 不可被丟掉
assert "evict：換代後不得殺掉新一代的 pane（它沒收過收尾任務）" \
  pane_alive "$pane_ev6_gen2"
assert_fails "evict：換代後不得寫 evicted 審計（沒收成就不能記成收了）" \
  grep -qE "Z evicted ev6 " "$DEV/agents.log"
assert "evict：換代拒絕後 locks/ 為空（die 路徑也要放鎖）" \
  test -z "$(ls -A "$DEV/locks")"
ab "$DEV" despawn ev6 >/dev/null 2>&1 || true
tmx kill-pane -t "$pane_ev6" 2>/dev/null || true
tmx kill-pane -t "$pane_ev6_gen2" 2>/dev/null || true

# 26e. 損壞 registry：出身不明就不能收——同 despawn/disposable 的 fail-closed
absp "$DEV" 0 spawn ev4 --runtime codex >/dev/null 2>&1
printf 'null\n' > "$DEV/agents/ev4.json"
before_t4="$(task_count "$DEV")"
assert_fails "evict：registry 是合法 JSON 但非 object → 拒絕（出身不明）" \
  ab "$DEV" evict ev4 --timeout 1
assert "evict：出身不明被拒時不送收尾任務" test "$(task_count "$DEV")" -eq "$before_t4"
assert "evict：出身不明被拒時 registry 未被清掉（留給人工處理）" \
  test -e "$DEV/agents/ev4.json"

# ---- 27. brief 正本的策略不變量（Phase 4）----
# 兩份 brief 是策略層的正本，機制對它們一無所知，所以只有這裡守得住：
# 條款被刪或被改回舊語意時要在測試就紅，而不是等某個 worker 真的把自己
# /clear 掉、脈絡連同 pane 一起消失才發現

# 27a. worker-brief：新語意的正面斷言
for kw in 'agent-bridge disposable' '你的 context 是資產' '收到收尾任務時' \
          '要不要再往下委派 subagent' '正是之後可能被追問的東西嗎'; do
  assert "worker brief 含 Phase 4 條款：$kw" grep -q -- "$kw" "$REPO_BRIEF"
done

# 27b. worker-brief：舊條款必須已經拔掉。
# 「reply 後清空 context」與「保留脈絡供追問」直接矛盾——照做的話 reply 完
# 脈絡就沒了，disposable 宣告與 evict 的收尾筆記全部失去意義
assert "worker brief 已不含「reply 後清空 context」語句（與保留脈絡矛盾）" \
  bash -c "! grep -q '清空自己的 context' '$REPO_BRIEF'"
assert "worker brief 不再叫 worker 自行 /clear" \
  bash -c "! grep -qE '完成 .reply. 後.*/clear' '$REPO_BRIEF'"

# 27c. orchestrator-brief：存在 + 策略要點
REPO_OBRIEF="$(dirname "$(dirname "$BRIDGE")")/share/orchestrator-brief.md"
assert "orchestrator brief 正本存在於 repo" test -r "$REPO_OBRIEF"
for kw in 'agent-bridge idle' 'agent-bridge evict' 'evicted-timeout' \
          '預設保留' 'AGENT_BRIDGE_MAX_SPAWN' \
          'Do not call the AgentTool'; do
  assert "orchestrator brief 含必要元素：$kw" grep -q -- "$kw" "$REPO_OBRIEF"
done

# ---- 28. gc：清舊 task，但三道保留線都不能破 ----
# tasks/ 只增不減不只是磁碟問題：idle 的回收決策直接掃這個目錄，資料越髒決策
# 越不可信（名稱重用那個 bug 的根因就是這裡）。但這是唯一會刪東西的命令，
# 失效方向必須一律偏向「留著」。
DGC="$TESTROOT/dgc"
mkdir -p "$DGC"/{agents,tasks,locks}

# mk_task <id> <status> <created_at> [pinned]
mk_task() {
  local id="$1" st="$2" ts="$3" pin="${4:-}"
  mkdir -p "$DGC/tasks/$id"
  jq -n --arg id "$id" --arg ts "$ts" --arg pin "$pin" \
    '{version:1, task_id:$id, from:"alice", to:"bob", created_at:$ts,
      updated_at:$ts, working_directory:"/tmp", status:"completed"}
     + (if $pin == "1" then {pinned: true} else {} end)' \
    > "$DGC/tasks/$id/metadata.json"
  printf '%s\n' "$st" > "$DGC/tasks/$id/status"
  printf 'x\n' > "$DGC/tasks/$id/request.md"
}
OLD_TS="2020-01-01T00:00:00Z"
NEW_TS="$(date -u +%FT%TZ)"
mk_task 20200101T000000Z-0a01 completed "$OLD_TS"
mk_task 20200101T000000Z-0a02 cancelled "$OLD_TS"
mk_task 20200101T000000Z-0a03 running   "$OLD_TS"
mk_task 20200101T000000Z-0a04 completed "$OLD_TS" 1
mk_task 20200101T000000Z-0a05 queued    "$OLD_TS"
mk_task 20260101T000000Z-0b01 completed "$NEW_TS"
# 判不出年紀的：壞掉的 created_at 不該讓它被當成很舊而刪掉
mk_task 20200101T000000Z-0a06 completed "not-a-timestamp"

# 28a. 預設是試算：一個都不能真的刪
gc_out="$(ab "$DGC" gc --older-than 1 2>/dev/null)"
assert "gc：預設只試算，不刪任何東西" \
  test -d "$DGC/tasks/20200101T000000Z-0a01"
assert "gc：試算列出可刪的舊終態 task" \
  bash -c "grep -q '20200101T000000Z-0a01' <<< '$gc_out'"
assert "gc：試算不列出未完成的 task" \
  bash -c "! grep -q '20200101T000000Z-0a03' <<< '$gc_out'"
assert "gc：試算不列出收尾筆記（pinned）" \
  bash -c "! grep -q '20200101T000000Z-0a04' <<< '$gc_out'"

# 28b. --apply：該刪的刪、三道保留線一條都不能破
ab "$DGC" gc --older-than 1 --apply >/dev/null 2>&1
assert "gc：--apply 刪掉夠舊的 completed" \
  test ! -d "$DGC/tasks/20200101T000000Z-0a01"
assert "gc：--apply 刪掉夠舊的 cancelled" \
  test ! -d "$DGC/tasks/20200101T000000Z-0a02"
# ★ 保留線一：進行中的任務不是垃圾，刪掉等於把還在跑的工作抹掉
assert "gc：running 的 task 不刪（未完成一律保留）" \
  test -d "$DGC/tasks/20200101T000000Z-0a03"
assert "gc：queued 的 task 不刪（還沒人取）" \
  test -d "$DGC/tasks/20200101T000000Z-0a05"
# ★ 保留線二：evict 的收尾筆記是這一層刻意留下的脈絡。它若會被 gc 清掉，
# 「上下文不會憑空消失」就只是延後兌現
assert "gc：pinned 的收尾筆記不刪（預設）" \
  test -d "$DGC/tasks/20200101T000000Z-0a04"
# ★ 保留線三：判不出年紀就不刪
assert "gc：created_at 壞掉的 task 不刪（判不出年紀就留著）" \
  test -d "$DGC/tasks/20200101T000000Z-0a06"
assert "gc：未滿保留期的 task 不刪" \
  test -d "$DGC/tasks/20260101T000000Z-0b01"
assert "gc：執行後 locks/ 為空（每個 task 各取各的鎖，不得殘留）" \
  test -z "$(ls -A "$DGC/locks")"

# 28c. --include-notes 才動筆記，且仍受其他兩條線約束
ab "$DGC" gc --older-than 1 --apply --include-notes >/dev/null 2>&1
assert "gc：--include-notes 才刪得掉收尾筆記" \
  test ! -d "$DGC/tasks/20200101T000000Z-0a04"
assert "gc：--include-notes 仍不刪未完成的 task" \
  test -d "$DGC/tasks/20200101T000000Z-0a03"

# 28d. 參數防線
assert_fails "gc：--older-than 非數字被拒" ab "$DGC" gc --older-than abc
assert_fails "gc：未知參數被拒" ab "$DGC" gc --bogus
# 目錄名不合 task-id 格式的東西一律不碰：這是唯一會 rm 的地方，寧可漏掉也不要
# 讓奇怪的名字走進 rm 的參數。用含空白的名字——TASK_ID_RE 允許 [A-Za-z0-9._-]，
# 普通目錄名都會通過，只有這類才測得到那道防線
stray="$DGC/tasks/bad name"
mkdir -p "$stray"
jq -n --arg ts "$OLD_TS" \
  '{version:1, task_id:"x", from:"a", to:"b", created_at:$ts, updated_at:$ts,
    working_directory:"/tmp", status:"completed"}' > "$stray/metadata.json"
printf 'completed\n' > "$stray/status"
ab "$DGC" gc --older-than 1 --apply >/dev/null 2>&1
assert "gc：目錄名不合 task-id 格式就不碰（即使其他條件都夠格被刪）" \
  test -d "$stray"

# 28e. evict 的收尾任務要真的被 pin 起來（不是只有測試資料會 pin）
DGP="$TESTROOT/dgcpin"
absp "$DGP" 0 spawn gp1 --runtime codex >/dev/null 2>&1
tid_gp="$(ab "$DGP" evict gp1 --timeout 1 2>/dev/null)"
assert "gc：evict 送出的收尾任務帶 pinned 標記（否則會被 gc 清掉）" \
  test "$(jq -r '.pinned // false' "$DGP/tasks/$tid_gp/metadata.json")" = true
# 一般 send 不該被 pin，否則 gc 永遠清不掉任何東西
ab "$DGP" register plain-a "$PANE_A" >/dev/null 2>&1
tid_pl="$(ab "$DGP" send plain-a --from alice --message "一般任務" 2>/dev/null)"
assert "gc：一般 send 不帶 pinned（否則 gc 等於失效）" \
  test "$(jq -r '.pinned // false' "$DGP/tasks/$tid_pl/metadata.json")" = false

# ---- 29. 第二輪獨立複核的修補（2026-07-23）----
DR2="$TESTROOT/drev2"
mkdir -p "$DR2"/{agents,tasks,locks}

# 29a. I1：直接 despawn 一個未宣告 disposable 的 worker，審計要看得出沒筆記。
# 機制上不擋（擋了會逼出 --force，習慣性加旗標等於沒有防線），但審計線不能
# 跟一次乾淨回收長得一模一樣
absp "$DR2" 0 spawn r1 --runtime codex >/dev/null 2>&1
ab "$DR2" despawn r1 >/dev/null 2>&1
assert "I1：未宣告 disposable 就直接 despawn → 審計記 despawned-unsaved" \
  grep -qE "Z despawned-unsaved r1 " "$DR2/agents.log"
# 宣告過的則是一次乾淨的回收，不該被記成有損失
absp "$DR2" 0 spawn r2 --runtime codex >/dev/null 2>&1
ab "$DR2" disposable r2 >/dev/null 2>&1
ab "$DR2" despawn r2 >/dev/null 2>&1
assert "I1：宣告過 disposable 的 despawn 記 despawned（不是 unsaved）" \
  grep -qE "Z despawned r2 " "$DR2/agents.log"
assert_fails "I1：宣告過的不得被記成 unsaved（否則記號分不出真正的損失）" \
  grep -qE "Z despawned-unsaved r2 " "$DR2/agents.log"
# evict 走過收尾流程，筆記已處理，不該再記 unsaved
absp "$DR2" 0 spawn r3 --runtime codex >/dev/null 2>&1
ab "$DR2" evict r3 --timeout 1 >/dev/null 2>&1
assert_fails "I1：evict 的 despawn 不記 unsaved（收尾流程已跑過）" \
  grep -qE "Z despawned-unsaved r3 " "$DR2/agents.log"

# 29b. I2：spawn_tag 空 → generation 綁定會整條失效，這時 evict 必須拒絕動作，
# 而不是帶著一個形同不存在的綁定往下走
pane_r4="$(absp "$DR2" 0 spawn r4 --runtime codex 2>/dev/null)"
jq 'del(.spawn_tag)' "$DR2/agents/r4.json" > "$DR2/agents/r4.tmp" \
  && mv "$DR2/agents/r4.tmp" "$DR2/agents/r4.json"
before_t_r4="$(task_count "$DR2")"
assert_fails "I2：registry 沒有 spawn_tag → evict 拒絕（綁定會失效）" \
  ab "$DR2" evict r4 --timeout 1
assert "I2：拒絕時不送收尾任務（不留孤兒）" \
  test "$(task_count "$DR2")" -eq "$before_t_r4"
assert "I2：拒絕時 pane 未被動" pane_alive "$pane_r4"
ab "$DR2" despawn r4 >/dev/null 2>&1 || true
tmx kill-pane -t "$pane_r4" 2>/dev/null || true

# 29c. I3：gc 不能刪掉「宣告已失效」的證據。idle 判斷宣告有沒有被後續任務推翻
# 靠的是掃 tasks/；那個任務被清掉，宣告就會從 expired 復活成 yes，
# orchestrator 據此直接回收一個其實已有新脈絡的 worker
DR3="$TESTROOT/drev3"
mkdir -p "$DR3"/{agents,tasks,locks}
absp "$DR3" 0 spawn r5 --runtime codex >/dev/null 2>&1
ab "$DR3" disposable r5 >/dev/null 2>&1
sleep 1
tid_r5="$(ab "$DR3" send r5 --from alice --message "宣告後的新任務" 2>/dev/null)"
ab "$DR3" receive "$tid_r5" >/dev/null 2>&1
ab "$DR3" reply "$tid_r5" --message "done" >/dev/null 2>&1
assert "I3：宣告後被派工 → idle 顯示 expired（前提）" \
  test "$(idle_field "$DR3" r5 3)" = expired
ab "$DR3" gc --older-than 0 --apply >/dev/null 2>&1
assert "I3：gc 不得刪掉讓宣告失效的那個任務（它是唯一的證據）" \
  test -d "$DR3/tasks/$tid_r5"
# ★ 核心斷言：gc 跑完之後，宣告不能復活
assert "I3：gc 之後 idle 仍是 expired（宣告不得復活成 yes）" \
  test "$(idle_field "$DR3" r5 3)" = expired
ab "$DR3" despawn r5 >/dev/null 2>&1 || true

# 29e. I6：despawn 走 stale 路徑時（registry 清掉了、但那個 pane 已不屬於這個
# agent，沒被回收）return 0，evict 不能把它當成回收成功而補一筆 evicted——
# 那會讓審計線宣稱發生過一次沒發生的回收。
# 模擬：回覆前把 registry 的 pane_id 指到另一個活著、但啟動指令不帶本 agent
# tag 的 pane
absp "$DR2" 0 spawn r6 --runtime codex >/dev/null 2>&1
(
  sleep 0.8
  for (( i = 0; i < 150; i++ )); do
    for d in "$DR2"/tasks/*/; do
      [[ -f "$d/metadata.json" ]] || continue
      [[ "$(jq -r '.to // ""' "$d/metadata.json" 2>/dev/null)" == r6 ]] || continue
      [[ "$(<"$d/status")" == queued ]] || continue
      jq --arg p "$PANE_A" '.pane_id = $p' "$DR2/agents/r6.json" \
        > "$DR2/agents/r6.tmp" && mv "$DR2/agents/r6.tmp" "$DR2/agents/r6.json"
      ab "$DR2" receive "$(basename "$d")" >/dev/null 2>&1
      ab "$DR2" reply "$(basename "$d")" --message "筆記" >/dev/null 2>&1
      exit 0
    done
    sleep 0.2
  done
) &
ab "$DR2" evict r6 --timeout 20 >/dev/null 2>&1
wait
assert "I6：despawn 走 stale（pane 未回收）時不得寫 evicted 審計" \
  bash -c "! grep -qE 'Z evicted r6 ' '$DR2/agents.log'"
assert "I6：stale 本身仍要留審計（註冊確實被清掉了）" \
  grep -qE "Z despawn-stale r6 " "$DR2/agents.log"
assert "I6：被指到的無辜 pane 不得被回收" pane_alive "$PANE_A"

# 29d. I5：gc 只碰 send 生成的形狀。公開的 TASK_ID_RE 連 `foo` 都算合法，
# 拿它當刪除門檻等於把任何人放進 tasks/ 的目錄都納入清理範圍
mkdir -p "$DR3/tasks/foo"
jq -n '{version:1, task_id:"foo", from:"a", to:"b",
        created_at:"2020-01-01T00:00:00Z", updated_at:"2020-01-01T00:00:00Z",
        working_directory:"/tmp", status:"completed"}' > "$DR3/tasks/foo/metadata.json"
printf 'completed\n' > "$DR3/tasks/foo/status"
ab "$DR3" gc --older-than 0 --apply >/dev/null 2>&1
assert "I5：不是 send 生成的目錄名不刪（唯一會 rm 的路徑要保守）" \
  test -d "$DR3/tasks/foo"

# ---- 總結 ----
printf '\n共 %d PASS、%d FAIL\n' "$PASS" "$FAIL"
if (( FAIL > 0 )); then
  exit 1
fi
exit 0
