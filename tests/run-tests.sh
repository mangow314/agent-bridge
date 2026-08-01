#!/usr/bin/env bash
# agent-bridge 測試（純 bash，零額外依賴）
# - 一律用 AGENT_BRIDGE_DATA 指向暫存目錄，不碰真實資料
# - tmux 整合測試只用獨立 socket：tmux -L agent-bridge-test -f /dev/null
set -u
unset TMUX
# 同理清掉 agent-bridge 自己的環境變數。測試套件的常態執行位置就是一個
# spawn／relay 出來的 worker pane（本專案自己 dogfood），那個 session 的
# AGENT_BRIDGE_SPAWN_TAG 與 AGENT_BRIDGE_RELAY_DEPTH 會被子行程一路繼承，
# 讓「hook 無 tag」「relay 首棒深度 0」這類前提悄悄失效——實測 2026-07-28：
# 同一份程式碼在人工起的 session 全綠、在 relay 出身的 session 卻有兩段
# 失敗，且失敗與被測程式無關。這是會隨執行環境改變結論的假綠／假紅來源，
# 比個別測例補 env -u 更該在源頭清掉：各測例要用這些變數時一律自己用 env
# 明確帶入，繼承來的值一概不要。
# shellcheck disable=SC2086  # 變數名不含空白，且無匹配時要展開成零個參數
unset ${!AGENT_BRIDGE_@}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BRIDGE="${BRIDGE:-$ROOT/bin/agent-bridge}"
# 源碼耦合檢查（§30 canary、§31i 寫入順序）抽取函式本體的對象。不跟著 $BRIDGE
# 走——$BRIDGE 是黑箱受測體，這兩處要的是**源碼**。M4 cutover 後正本是 Rust，
# 故預設 rust；`SRC_KIND=bash` 可切回 bash 正本（rollback 期與雙實作對照用）。
# 顯式的 kind＋path 是 M3 codex 複核的建議形（source-kind／source-path）。
# 光有 kind 還不夠：$BRIDGE（黑箱受測體）與 SRC_KIND（源碼抽取對象）若各指
# 一套實作，兩邊各自全綠、整套照樣綠，等於沒驗到對應關係（獨立複核
# 2026-07-31 實證）。故 kind 先驗 enum，再與載具實測結果交叉比對。
SRC_KIND="${SRC_KIND:-rust}"
case "$SRC_KIND" in
  rust|bash) ;;
  *) echo "SRC_KIND 需為 rust 或 bash：$SRC_KIND" >&2; exit 1 ;;
esac
SRC_BASH="$ROOT/bin/agent-bridge.bash"
SRC_NOTIFY_RS="$ROOT/crates/ab-core/src/notify.rs"
SRC_TASK_RS="$ROOT/crates/ab-core/src/task.rs"
# 載具是哪套實作：Rust 認得 `__implemented-commands`（rc 0）、bash 正本當成
# 未知指令（rc 1）——用既有介面判別，不為了自報身分再開一個。探針的 DATA
# 指到不可建立的路徑，免得 bash 分支順手建到使用者真實的資料目錄
if AGENT_BRIDGE_DATA=/dev/null/probe "$BRIDGE" __implemented-commands \
    >/dev/null 2>&1; then
  BRIDGE_KIND=rust
else
  BRIDGE_KIND=bash
fi
if [[ "$BRIDGE_KIND" != "$SRC_KIND" ]]; then
  echo "SRC_KIND=$SRC_KIND 與 BRIDGE 不匹配（載具偵測為 $BRIDGE_KIND）：$BRIDGE" >&2
  exit 1
fi
SOCK="agent-bridge-test"

if ! REAL_TMUX="$(command -v tmux)"; then
  echo "跑測試需要 tmux" >&2
  exit 1
fi

# 真 claude 執行檔要在 PATH 被 shim 汙染前解析（後面 spawn 測試會把假
# claude/codex 前置進 PATH，屆時 command -v claude 只找得到 stub）。
# CC canary（測例 30）用這個值，找不到就跳過
REAL_CLAUDE="$(command -v claude 2>/dev/null || true)"

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

# evt_reason <events.log> <reason>：notify-failed 行帶 reason=<v>（HOOK-NOTIFY-4）。
# 四個關卡原本共用同一個 notify-failed，事後分不出誰擋的。
#
# 比對**整行**而非「行內某處有 reason=」：reason 的契約是 additive——append 在
# `pane=`／`cmd=` 之後、既有欄位順序不動。鬆散比對下，把欄位重排成
# `reason=… pane=… cmd=…` 照樣全綠，additive 那半條契約等於沒鎖。
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
evt_reason() {
  local log="$1" r="$2"
  grep -qE "^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]+Z notify-failed pane=[^[:space:]]+ cmd=[^[:space:]]+ reason=${r}\$" "$log"
}

# assert_reason <desc> <events.log> <reason>：bash 正本自 M4 凍結、不記 reason，
# 故 SRC_KIND=bash 顯式 SKIP（不計 PASS／FAIL，bash 側總數不變）。
assert_reason() {
  if [[ "$SRC_KIND" == bash ]]; then
    printf 'SKIP: %s（bash 正本凍結，未實作 reason 欄位）\n' "$1"
  else
    assert "$1" evt_reason "$2" "$3"
  fi
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
env > "$TESTROOT/$rt-env.txt"
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
# agy 同理（真 agy 也可能在 PATH 上）。shim 無條件建立、只有分組 37 會用到：
# bash 正本不支援 agy runtime，多這個檔對它是死碼而非行為差異
make_runtime_shim agy

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

# state_field_is <state-file> <field> <expect>：讀 state/<name>.json 某欄位是否
# 等於期望值（讀不到／解析失敗即回 false，天然涵蓋「state 檔還沒被 hook 寫出」）
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
state_field_is() {
  local file="$1" field="$2" expect="$3"
  [[ "$(jq -r --arg f "$field" '.[$f] // empty' "$file" 2>/dev/null)" == "$expect" ]]
}
# now_iso_test：產生一個「現在」的 UTC ISO 8601 字串，供測試偽造新鮮的 state 檔
# shellcheck disable=SC2329  # 經測試片段直接呼叫
now_iso_test() { date -u +%Y-%m-%dT%H:%M:%SZ; }
# hookcall <data-dir> <spawn-tag> <event> <stdin-json>：直接呼叫 hook 子命令，
# 不經 tmux；stdin 走 heredoc 餵 fixture JSON，stdout/exit code 由呼叫端擷取
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
hookcall() {
  local data="$1" tag="$2" event="$3" json="$4"
  printf '%s' "$json" | env AGENT_BRIDGE_DATA="$data" AGENT_BRIDGE_SPAWN_TAG="$tag" \
    PATH="$SHIM:$PATH" "$BRIDGE" hook "$event"
}

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
# spec: CLI-REGISTER-1 CLI-LIST-1 STATE-AGENT-1 STATE-AGENT-3 STATE-GEN-1 ENV-DATA-1
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
# spec: CLI-SEND-2 CLI-SEND-3 CLI-GEN-1 STATE-GEN-3
out="$(ab "$D1" send nobody --from alice --message hi 2>/dev/null)"; rc=$?
assert "send 未註冊 agent：非零退出" test "$rc" -ne 0
assert "send 未註冊 agent：stdout 為空" test -z "$out"
assert_fails "send 缺 --from 報錯" ab "$D1" send alice --message hi
assert_fails "send 缺訊息參數報錯" ab "$D1" send alice --from bob
assert_fails "send：--from 名稱不合法被拒" \
  ab "$D1" send alice --from "bad name" --message hi

# ---- 3. 未知 task 的 receive / status / read ----
# spec: CLI-GEN-1 CLI-GEN-3 CLI-RECEIVE-1 CLI-STATUS-1
assert_fails "receive 未知 task 報錯" ab "$D1" receive 20990101T000000Z-dead
assert_fails "status 未知 task 報錯"  ab "$D1" status  20990101T000000Z-dead
assert_fails "read 未知 task 報錯"    ab "$D1" read    20990101T000000Z-dead

# ---- 4. send 快樂路徑 + read 於未 completed ----
# spec: CLI-SEND-1 CLI-GEN-2 CLI-RECEIVE-1 STATE-TASK-1 STATE-TASK-2 STATE-TASK-3
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
# spec: CLI-REPLY-1 STATE-TASK-4
ab "$D2" reply "$id2" --message "premature" >/dev/null 2>&1; rc=$?
assert "reply 於 queued（未 receive）：非零退出" test "$rc" -ne 0
assert "reply 於 queued：狀態檔不變（仍 queued）" st_is "$D2" "$id2" queued
assert "reply 於 queued：不產生 response.md" \
  test ! -e "$D2/tasks/$id2/response.md"

# ---- 6. 特殊字元 byte-for-byte 保真（--message-file 與 stdin） ----
# spec: CLI-SEND-1
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
# spec: CLI-REPLY-1 CLI-READ-1 STATE-TASK-4
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
# spec: CLI-SEND-3
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
# spec: HOOK-NOTIFY-2 ENV-NOTIFY-1
# notify_pane 送鍵前 capture-pane 掃 Claude Code 權限對話框特徵，掃到就降級。這裡
# 直接鎖住核心不變量「攔截時一個按鍵都沒送進 pane」——而非只驗 return code：假 pane
# 印出對話框特徵後進 `while read` 迴圈、把收到的每一行記進 got 檔。攔截生效＝got 不
# 含通知文字。為排除「其實送了、只是還沒 append」的偽陰性，先送一個哨兵行、確認 got
# 記得到它（證明記錄機制活著），再斷言 got 裡沒有 task-id。
# 註：capture 失敗 fail-closed（B1）與送 Enter 前二次掃描（B3）真 tmux 難穩定構造
# （「capture 失敗但 send-keys 成功」與「文字送出後才彈框」），由 8a④/8a⑤ 的
# 有狀態 tmux shim 決定性鎖住。
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
# shellcheck disable=SC2016  # $1/$2 由內層 bash 展開，刻意單引號
assert "對話框偵測(Bash 框)：got 恰好只有哨兵一行（通知的文字與 Enter 都沒送進來）" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -Fxq "$2" "$1"' _ "$TESTROOT/ask1-got.txt" 'SENTINEL-ask1'
assert "對話框偵測(Bash 框)：events.log 記 notify-failed" \
  evt_grep "$D5a/tasks/$id5a/events.log" notify-failed
assert_reason "對話框偵測(Bash 框)：notify-failed 標 reason=prompt" \
  "$D5a/tasks/$id5a/events.log" prompt
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
# shellcheck disable=SC2016  # 同上，內層 bash 展開
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

# 8a③b matcher 位置有錨（2026-08-01 收窄）：畫面**上半部**出現特徵字串、下緣是
# 正常內容時，MUST 照送。這是 production 形狀而非人造刁難——一個正在調查這個
# bug 的 coordinator，pane 上就有 `rg 'Requesting permission for:|Do you want to
# proceed'` 的指令回顯，一行湊齊三組特徵；實測誤判 19/24≈79%，且 P4 之後同一
# matcher 餵給 TUI 的 BLOCKER 軸，會變成常駐假 ⛔blocked。
# bash 正本自 M4 凍結（仍是整屏無錨比對，會誤判），故 SRC_KIND=bash 顯式 SKIP。
if [[ "$SRC_KIND" == bash ]]; then
  printf 'SKIP: 8a③b matcher 下緣錨（bash 正本凍結，仍是整屏無錨比對）\n'
else
  D5j="$TESTROOT/d5j"
  p_talk="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
  ab "$D5j" register tata "$p_talk" 2>/dev/null
  # 特徵行在上，其後 20 行正常內容把它推出下緣掃描區
  tmx send-keys -t "$p_talk" \
    "printf '%s\n' 'rg \"Requesting permission for:|Do you want to proceed?|esc to cancel|Esc to cancel\" notes.md' ; for i in \$(seq 1 20) ; do printf 'ordinary worker output line %s\n' \"\$i\" ; done ; touch $TESTROOT/talk-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/talk-got.txt ; done" Enter
  wait_for 10 test -f "$TESTROOT/talk-ready"
  # 前置斷言：特徵字串真的還在可見一屏內（否則本組退化成 8a③ 的複本）
  tmx capture-pane -pJ -t "$p_talk" > "$TESTROOT/talk-screen.txt"
  assert "8a③b 前置：特徵字串確實在可見一屏（只是不在下緣）" \
    grep -qF -- 'Requesting permission for:' "$TESTROOT/talk-screen.txt"
  id5j="$(ab "$D5j" send tata --from alice --message hi 2>/dev/null)"
  assert "8a③b 談論權限框的畫面：pane 收到通知文字（不再誤判）" \
    wait_for 10 grep -q "$id5j" "$TESTROOT/talk-got.txt"
  assert_fails "8a③b 談論權限框的畫面：events.log 不記 notify-failed" \
    evt_grep "$D5j/tasks/$id5j/events.log" notify-failed
  tmx kill-pane -t "$p_talk" 2>/dev/null || true
fi

# 8a④ B1 regression（有狀態 shim）：「capture-pane 失敗但 send-keys 可用」在真
# tmux 幾乎構造不出來（pane 活著時 capture 不會失敗），shim 讓 capture 一律非零、
# send-keys 只記 argv 不透傳。鎖住 fail-closed 方向：查不到 pane 狀態必須整個放棄
# 送鍵，不得退化成「capture 失敗就當沒對話框」的 fail-open——那會讓防線被整個
# 略過（複核 B1）。marker 檔證明 capture 真的被呼叫且被 shim 攔截，排除「PATH
# 沒生效、其實走了正常路徑」的偽陰性。
D5d="$TESTROOT/d5d"
CAPSHIM="$TESTROOT/capshim"
mkdir -p "$CAPSHIM"
cat > "$CAPSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
if [[ "\$1" == "capture-pane" ]]; then
  touch $(printf '%q' "$TESTROOT/b1-cap-hit")
  echo "capture failed (test stub)" >&2; exit 1
fi
if [[ "\$1" == "send-keys" ]]; then
  { printf '%s ' "\$@"; printf '\n'; } >> $(printf '%q' "$TESTROOT/b1-sendkeys.log")
  exit 0
fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$CAPSHIM/tmux"
env AGENT_BRIDGE_DATA="$D5d" PATH="$CAPSHIM:$PATH" "$BRIDGE" \
  register kaka "$PANE_B" 2>/dev/null
id5d="$(env AGENT_BRIDGE_DATA="$D5d" PATH="$CAPSHIM:$PATH" "$BRIDGE" \
  send kaka --from alice --message hi 2>/dev/null)"; rc=$?
assert "capture 失敗 fail-closed(B1)：send 仍 exit 0（訊息留 mailbox）" test "$rc" -eq 0
assert "capture 失敗 fail-closed(B1)：capture 確實被呼叫且失敗（shim 生效證據）" \
  test -f "$TESTROOT/b1-cap-hit"
assert "capture 失敗 fail-closed(B1)：send-keys 一次都沒被呼叫" \
  test ! -e "$TESTROOT/b1-sendkeys.log"
assert "capture 失敗 fail-closed(B1)：events.log 記 notify-failed" \
  evt_grep "$D5d/tasks/$id5d/events.log" notify-failed
# capture 讀不出來歸 pane-gone 桶（「pane 狀態查不到」與「pane 真的不在」同處置）
assert_reason "capture 失敗 fail-closed(B1)：notify-failed 標 reason=pane-gone" \
  "$D5d/tasks/$id5d/events.log" pane-gone

# 8a⑤ B3 regression（有狀態 shim）：第一次掃描時畫面乾淨、通知文字送出後的延遲
# 期間 worker 才彈出權限框——真 tmux 難穩定構造這個時序，shim 用計數檔讓第一次
# capture 回正常畫面、第二次回對話框特徵。鎖住二次掃描：send-keys 恰好只被呼叫
# 一次（文字那次，行內含 task-id），Enter 那次必須被攔下；刪掉二次掃描的
# regression 會讓 log 多出 Enter 行（wc 變 2）、拿掉整段延遲重掃則 capcount
# 停在 1，兩條斷言互為犄角（複核 B3）。
D5e="$TESTROOT/d5e"
RACESHIM="$TESTROOT/raceshim"
mkdir -p "$RACESHIM"
cat > "$RACESHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
if [[ "\$1" == "capture-pane" ]]; then
  n=\$(cat $(printf '%q' "$TESTROOT/b3-capcount") 2>/dev/null || echo 0)
  n=\$((n+1)); printf '%s' "\$n" > $(printf '%q' "$TESTROOT/b3-capcount")
  if [[ "\$n" -eq 1 ]]; then
    printf '%s\n' 'just some normal worker output'
  else
    printf '%s\n' 'Do you want to proceed?' 'Esc to cancel · Tab to amend'
  fi
  exit 0
fi
if [[ "\$1" == "send-keys" ]]; then
  { printf '%s ' "\$@"; printf '\n'; } >> $(printf '%q' "$TESTROOT/b3-sendkeys.log")
  exit 0
fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$RACESHIM/tmux"
env AGENT_BRIDGE_DATA="$D5e" PATH="$RACESHIM:$PATH" "$BRIDGE" \
  register keke "$PANE_B" 2>/dev/null
id5e="$(env AGENT_BRIDGE_DATA="$D5e" AGENT_BRIDGE_NOTIFY_DELAY=0.05 \
  PATH="$RACESHIM:$PATH" "$BRIDGE" send keke --from alice --message hi 2>/dev/null)"
assert "延遲中彈框(B3)：兩次掃描都發生（capture 恰被呼叫 2 次）" \
  test "$(cat "$TESTROOT/b3-capcount" 2>/dev/null)" = 2
# shellcheck disable=SC2016  # 同上，內層 bash 展開
assert "延遲中彈框(B3)：send-keys 恰好一次且是通知文字（Enter 被二次掃描攔下）" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -q "$2" "$1"' _ \
  "$TESTROOT/b3-sendkeys.log" "$id5e"
assert "延遲中彈框(B3)：events.log 記 notify-failed" \
  evt_grep "$D5e/tasks/$id5e/events.log" notify-failed

# 8a⑥ plan mode 退出確認框：標題不含「Do you want to 」、footer 不含「Esc to
# cancel」，第一組特徵比對不到；它的 Enter 預設是「Yes, and use auto mode」，
# 誤觸比權限框更糟（批准 plan＋切 auto mode）。特徵是標題行的兩個片段 AND
# （2026-07-23 真 UI 實測，文案見 screen_has_prompt 註解）
D5f="$TESTROOT/d5f"
p_plan="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D5f" register pupu "$p_plan" 2>/dev/null
tmx send-keys -t "$p_plan" \
  "printf '%s\n' 'Claude has written up a plan and is ready to execute. Would you like to proceed?' ; touch $TESTROOT/ask3-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/ask3-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/ask3-ready"
id5f="$(ab "$D5f" send pupu --from alice --message hi 2>/dev/null)"
tmx send-keys -t "$p_plan" 'SENTINEL-ask3' Enter
assert "對話框偵測(plan 框)：記錄機制活著（哨兵行有進 got）" \
  wait_for 10 grep -q 'SENTINEL-ask3' "$TESTROOT/ask3-got.txt"
# shellcheck disable=SC2016  # $1/$2 由內層 bash 展開，刻意單引號
assert "對話框偵測(plan 框)：got 恰好只有哨兵一行（plan 框也一個按鍵都沒送）" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -Fxq "$2" "$1"' _ "$TESTROOT/ask3-got.txt" 'SENTINEL-ask3'
assert "對話框偵測(plan 框)：events.log 記 notify-failed" \
  evt_grep "$D5f/tasks/$id5f/events.log" notify-failed
tmx kill-pane -t "$p_plan" 2>/dev/null || true

# 8a⑥b 折行版 plan 框：窄 pane 下 TUI word-wrap 會把 80 字元的標題拆行，特徵
# 片段「Would you like to proceed」被折點切開。逐行 grep 在這種畫面必偽陰性
# （複核以 fold 反例證實），靠 screen_has_prompt 的空白正規化拼回。fixture 直接
# 印拆行後的兩行，鎖住「折行的 plan 框也攔得住」
D5h="$TESTROOT/d5h"
p_wrap="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D5h" register wawa "$p_wrap" 2>/dev/null
tmx send-keys -t "$p_wrap" \
  "printf '%s\n' 'Claude has written up a plan and is ready to execute. Would you like to' 'proceed?' ; touch $TESTROOT/ask4-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/ask4-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/ask4-ready"
id5h="$(ab "$D5h" send wawa --from alice --message hi 2>/dev/null)"
tmx send-keys -t "$p_wrap" 'SENTINEL-ask4' Enter
assert "對話框偵測(折行 plan 框)：記錄機制活著（哨兵行有進 got）" \
  wait_for 10 grep -q 'SENTINEL-ask4' "$TESTROOT/ask4-got.txt"
# shellcheck disable=SC2016  # $1/$2 由內層 bash 展開，刻意單引號
assert "對話框偵測(折行 plan 框)：got 恰好只有哨兵一行（折行拆散片段仍攔住）" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -Fxq "$2" "$1"' _ "$TESTROOT/ask4-got.txt" 'SENTINEL-ask4'
assert "對話框偵測(折行 plan 框)：events.log 記 notify-failed" \
  evt_grep "$D5h/tasks/$id5h/events.log" notify-failed
tmx kill-pane -t "$p_wrap" 2>/dev/null || true

# 8a⑦ 半特徵放行對照：plan 框特徵是兩片段 AND，任一片段單獨出現在普通輸出
# （worker 引述、討論文字）都不得誤攔——兩個片段各測一次，缺一則「實作退化成
# 只檢查另一片段」的 mutation 抓不到（複核 B2）
D5g="$TESTROOT/d5g"
p_half="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D5g" register haha "$p_half" 2>/dev/null
tmx send-keys -t "$p_half" \
  "printf '%s\n' 'Would you like to proceed with the deployment?' ; touch $TESTROOT/half-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/half-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/half-ready"
id5g="$(ab "$D5g" send haha --from alice --message hi 2>/dev/null)"
assert "半特徵畫面(片段二)：pane 收到通知文字（單片段不誤攔）" \
  wait_for 10 grep -q "$id5g" "$TESTROOT/half-got.txt"
assert_fails "半特徵畫面(片段二)：通知未降級（events.log 不記 notify-failed）" \
  evt_grep "$D5g/tasks/$id5g/events.log" notify-failed
tmx kill-pane -t "$p_half" 2>/dev/null || true

D5i="$TESTROOT/d5i"
p_half1="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D5i" register hoho "$p_half1" 2>/dev/null
tmx send-keys -t "$p_half1" \
  "printf '%s\n' 'the agent has written up a plan for later review' ; touch $TESTROOT/half1-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/half1-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/half1-ready"
id5i="$(ab "$D5i" send hoho --from alice --message hi 2>/dev/null)"
assert "半特徵畫面(片段一)：pane 收到通知文字（單片段不誤攔）" \
  wait_for 10 grep -q "$id5i" "$TESTROOT/half1-got.txt"
assert_fails "半特徵畫面(片段一)：通知未降級（events.log 不記 notify-failed）" \
  evt_grep "$D5i/tasks/$id5i/events.log" notify-failed
tmx kill-pane -t "$p_half1" 2>/dev/null || true

# ---- 8b. 鎖失敗路徑：權限失敗 vs 真正鎖佔用 ----
# spec: STATE-LOCK-1
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
# spec: CLI-UNREGISTER-1
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
# spec: CLI-START-1
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
# spec: CLI-FAIL-1
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
# spec: CLI-CANCEL-1
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
# spec: CLI-AWAIT-1 CLI-AWAIT-2 ENV-POLL-1
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
# spec: CLI-SEND-1 CLI-RECEIVE-1 CLI-REPLY-1
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
# spec: STATE-TASK-1

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
# spec: CLI-SPAWN-1 CLI-SPAWN-2 CLI-SPAWN-3 CLI-SPAWN-4 ENV-SPAWN-1 ENV-HOOKS-1 ENV-PASS-1 ENV-TAG-1 HOOK-BIND-1 STATE-AGENT-1

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
  grep -qE "Z spawned w1 ${pane_w1} codex -\$" "$DSPAWN/agents.log"
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
#     也仍適用；詳見 README.zh-TW.md，那段因果曾兩度寫錯）
#   - 混進 -p/--print 會讓 pane 跑完即退，worker 根本不存在
#   - 混進 --setting-sources 會讓 worker 脫離使用者的全域安全設定（--settings
#     不算違反這條：hooks 分層是合併不是覆蓋，全域規則原樣生效，只是外加一份
#     隨 repo 走的 worker 專屬 hooks 版本，理由見 bin/agent-bridge 的
#     CLAUDE_HOOKS_SETTINGS 常數註解）
# 關鍵是**精確的完整參數集合**而非子字串：獨立複核以 mutation 反例證明，
# 只比對子字串時 `--permission-mode auto --permission-mode bypassPermissions
# --setting-sources project <prompt>` 這種 argv 能讓所有斷言全綠——後面偷偷
# 追加的旗標才是真正生效的那個。白名單式斷言（恰好五個參數，理由與更新見
# 下方 16a2 白名單斷言的註解）比逐條列黑名單更強：任何多餘旗標都會讓行數對不上。
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
  grep -q -- 'The above is your worker brief' "$TESTROOT/claude-args.txt"
# 白名單：argv 必須「恰好」是 --permission-mode / auto / --settings / <path> /
# <prompt> 五個。這條才是真正擋住「追加旗標」的防線，上面幾條子字串斷言擋不住。
# 數字從「恰好三個」變成「恰好五個」是通知原生化 Phase 2 的設計異動：claude
# 分支無條件加上 `--settings <CLAUDE_HOOKS_SETTINGS>` 注入 worker hooks（見
# bin/agent-bridge 的 CLAUDE_HOOKS_SETTINGS 常數註解）——這是白名單契約隨新
# 旗標同步更新，不是放寬 exact-count 判準；仍是逐 index 比對＋恰好五個，任何
# 「追加」旗標一樣會讓行數對不上
assert "claude runtime argv 恰好五個參數（含 --settings；追加任何旗標都該紅）" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/claude-argv.txt'; (( \${#A[@]} == 5 ))"
assert "claude runtime argv 前四個恰為 --permission-mode auto --settings <path>、第五個是 prompt 非旗標" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/claude-argv.txt'; [[ \${A[0]} == '--permission-mode' && \${A[1]} == 'auto' && \${A[2]} == '--settings' && \${A[3]} == '$ROOT/share/claude-worker-hooks.json' && \${A[4]} != -* ]]"
assert "claude runtime 探針重送生效：ready == true" \
  jq -e '.ready == true' "$DSPAWN/agents/wc1.json"
assert "agents.log 記 spawned wc1 … claude" \
  grep -qE "Z spawned wc1 ${pane_wc} claude -\$" "$DSPAWN/agents.log"
assert "claude runtime worker 可正常 despawn" ab "$DSPAWN" despawn wc1

# 16a3. spawn --model：模型下放。值會進 tagged_cmd（pane 啟動命令字串），
# 與 brief 路徑同級暴露面，防線兩條：字元集擋 sh/tmux 分隔符（命令注入）、
# 首字元強制英數（旗標走私——`--model --bare` 不擋的話就是往 worker 啟動
# 旗標塞任意開關的後門）。argv 斷言沿用 16a2 的教訓走白名單：鎖「恰好七個
# 參數」（16a2 的五個 ＋ --model <m> 兩個，--settings 同樣無條件在場），
# 子字串斷言擋不住偷偷追加的旗標。數字從「恰好五個」變成「恰好七個」同樣是
# Phase 2 新增 --settings 造成的契約更新，理由見 16a2 上方註解，不重複
absp "$DSPAWN" 20 spawn wm1 --runtime claude --model sonnet-t.0 >/dev/null 2>&1; rc=$?
assert "spawn --model：exit 0" test "$rc" -eq 0
assert "claude argv 恰好七個參數（--permission-mode auto --settings <path> --model <m> <prompt>）" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/claude-argv.txt'; (( \${#A[@]} == 7 ))"
assert "claude argv 的 --settings 與 --model 值皆就位、prompt 仍是最後一個非旗標參數" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/claude-argv.txt'; [[ \${A[2]} == '--settings' && \${A[3]} == '$ROOT/share/claude-worker-hooks.json' && \${A[4]} == '--model' && \${A[5]} == 'sonnet-t.0' && \${A[6]} != -* ]]"
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
# 「恰好五個參數」白名單鎖住，這裡不重複（w1 是 16a 留下的無 --model spawn）
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

# 16a4. proxy 環境穿透：pane 的環境繼承自 tmux server 而非 spawn 呼叫者，
# orchestrator shell 的 proxy 變數必須拼進啟動指令才到得了 worker（受限網路
# 實測：runtime 直連被 MITM 擋下，第一隻 worker 陣亡）。env -u 清場＋
# sentinel 值讓斷言不受宿主機真實 proxy 設定影響。no_proxy 值刻意帶空白與
# 逗號：實作若退化成不跳脫（丟掉 printf %q），啟動指令在此拆詞、往返必損，
# 兩層斷言（啟動指令片段＋worker 程序實拿的值）都會紅
rm -f "$TESTROOT/claude-env.txt"
env -u http_proxy -u all_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u NO_PROXY \
    https_proxy='http://sentinel:1' no_proxy='st a,b' \
    AGENT_BRIDGE_DATA="$DSPAWN" AGENT_BRIDGE_READY_TIMEOUT=20 \
    AGENT_BRIDGE_READY_PROBE_INTERVAL=0.5 PATH="$SHIM:$PATH" \
    "$BRIDGE" spawn wp1 --runtime claude >/dev/null 2>&1; rc=$?
assert "spawn（帶 proxy 環境）：exit 0" test "$rc" -eq 0
wp1_cmd="$(tmx display -pt "$(jq -r .pane_id "$DSPAWN/agents/wp1.json")" '#{pane_start_command}')"
# tmux 存 pane_start_command 除了整條加雙引號，還會把反斜線跳脫成 \\
# （實測 3.7b，獨立 socket + cat -A 驗證），故期望片段裡的 %q 反斜線要寫雙份
wp1_frag=' https_proxy=http://sentinel:1 no_proxy=st\\ a\\,b exec '
assert "啟動指令帶跳脫後的 proxy 前綴，且 tag 仍是第一個 token" \
  bash -c "c=${wp1_cmd@Q}; f=${wp1_frag@Q}; c=\"\${c#\\\"}\"; [[ \"\$c\" == AGENT_BRIDGE_SPAWN_TAG=ab-spawn-wp1-* && \"\$c\" == *\"\$f\"* ]]"
assert "未設定的 proxy 變數不被注入啟動指令" \
  bash -c "c=${wp1_cmd@Q}; [[ \"\$c\" != *' http_proxy='* && \"\$c\" != *' all_proxy='* && \"\$c\" != *' HTTP_PROXY='* && \"\$c\" != *' HTTPS_PROXY='* && \"\$c\" != *' ALL_PROXY='* && \"\$c\" != *' NO_PROXY='* ]]"
assert "worker 程序環境實拿 https_proxy（sentinel 值完整）" \
  grep -Fxq 'https_proxy=http://sentinel:1' "$TESTROOT/claude-env.txt"
assert "worker 程序環境實拿含空白逗號的 no_proxy（%q 跳脫往返無損）" \
  grep -Fxq 'no_proxy=st a,b' "$TESTROOT/claude-env.txt"
assert "帶 proxy 前綴的 worker 可正常 despawn（tag 綁定比對不受影響）" \
  ab "$DSPAWN" despawn wp1

# 16a5. AGENT_BRIDGE_PASS_ENV：白名單版的環境穿透。與 16a4 同一條理由（pane 繼承
# tmux server 而非呼叫者），差別是變數由呼叫端指名。典型用途是 headless 姿態旗標
# （例如 CLAUDE_UNATTENDED）——那類變數沒跟過去，pane 會靜默退回有人值守的寬鬆
# 姿態，比明確失敗難察覺。值同樣刻意帶空白與逗號驗 %q 跳脫往返。
rm -f "$TESTROOT/claude-env.txt"
env AGENT_BRIDGE_DATA="$DSPAWN" AGENT_BRIDGE_READY_TIMEOUT=20 \
    AGENT_BRIDGE_READY_PROBE_INTERVAL=0.5 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_PASS_ENV='PE_SET,PE_UNSET' PE_SET='v 1,x' \
    "$BRIDGE" spawn wp2 --runtime claude >/dev/null 2>&1; rc=$?
assert "spawn（帶 PASS_ENV）：exit 0" test "$rc" -eq 0
wp2_cmd="$(tmx display -pt "$(jq -r .pane_id "$DSPAWN/agents/wp2.json")" '#{pane_start_command}')"
wp2_frag=' PE_SET=v\\ 1\\,x '
assert "指名且已設的變數進啟動指令（%q 跳脫），tag 仍是第一個 token" \
  bash -c "c=${wp2_cmd@Q}; f=${wp2_frag@Q}; c=\"\${c#\\\"}\"; [[ \"\$c\" == AGENT_BRIDGE_SPAWN_TAG=ab-spawn-wp2-* && \"\$c\" == *\"\$f\"* ]]"
assert "指名但未設的變數不塞空值進啟動指令" \
  bash -c "c=${wp2_cmd@Q}; [[ \"\$c\" != *' PE_UNSET='* ]]"
assert "worker 程序環境實拿 PE_SET（含空白逗號，往返無損）" \
  grep -Fxq 'PE_SET=v 1,x' "$TESTROOT/claude-env.txt"
assert "spawn 不是接力鏈的一環，不下傳 relay 深度" \
  bash -c "c=${wp2_cmd@Q}; [[ \"\$c\" != *AGENT_BRIDGE_RELAY_DEPTH=* ]]"
assert "帶 PASS_ENV 前綴的 worker 可正常 despawn" ab "$DSPAWN" despawn wp2
assert_fails "PASS_ENV 含不合法變數名被拒（擋往啟動指令拼接的意外詞）" \
  env AGENT_BRIDGE_DATA="$DSPAWN" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
      AGENT_BRIDGE_PASS_ENV='OK_ONE,bad-name' \
    "$BRIDGE" spawn wp3 --runtime claude
assert "PASS_ENV 被拒：不留 registry" test ! -e "$DSPAWN/agents/wp3.json"

# 16a6. claude worker hooks settings 注入（通知原生化 Phase 2）：--settings
# 無條件加在 claude 分支，讀不到就必須在建 pane 之前 die——同 WORKER_BRIEF 預檢
# 的 fail-closed 理由，pane 落地後才死會留下一個佔 cap 的孤兒 worker。codex
# 分支完全不受影響：它不吃這個旗標，不該被這份 claude 專屬檔案的缺失擋下。

# 16a6-1：runtime=claude 的 pane_start_command 含 --settings 且路徑正確
pane_hk1="$(absp "$DSPAWN" 20 spawn hk1 --runtime claude 2>/dev/null)"; rc=$?
assert "spawn（hooks 快樂路徑）--runtime claude：exit 0" test "$rc" -eq 0
hk1_cmd="$(tmx display -pt "$pane_hk1" '#{pane_start_command}')"
assert "claude runtime pane_start_command 含 --settings 且路徑正確" \
  bash -c "c=${hk1_cmd@Q}; [[ \"\$c\" == *'--settings '*'$ROOT/share/claude-worker-hooks.json'* ]]"
assert "hooks 快樂路徑 worker 可正常 despawn" ab "$DSPAWN" despawn hk1

# 16a6-2：runtime=codex 的 pane_start_command 不含 --settings（確認未誤植；
# w1_cmd 是 §16a 留下的 codex pane，這裡直接複用不必再開一個 pane）
assert "codex runtime pane_start_command 不含 --settings（確認未誤植）" \
  bash -c "c=${w1_cmd@Q}; [[ \"\$c\" != *--settings* ]]"

# 16a6-3：hooks 檔缺失＋runtime=claude → die，且建 pane 前就失敗（無孤兒、無 registry）
DHKMISS="$TESTROOT/dhkmiss"
mkdir -p "$DHKMISS/agents" "$DHKMISS/locks" "$DHKMISS/tasks"
before_hkmiss="$(pane_count)"
env AGENT_BRIDGE_DATA="$DHKMISS" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_CLAUDE_HOOKS="$TESTROOT/no-such-hooks.json" \
  "$BRIDGE" spawn hkm --runtime claude >/dev/null 2>"$TESTROOT/hkmiss.err"; rc=$?
assert "hooks 檔缺失：非零退出" test "$rc" -ne 0
assert "hooks 檔缺失：訊息指出可用 AGENT_BRIDGE_CLAUDE_HOOKS 覆蓋" \
  grep -q 'AGENT_BRIDGE_CLAUDE_HOOKS' "$TESTROOT/hkmiss.err"
assert "hooks 檔缺失：不建 pane（建 pane 前就死，無孤兒）" \
  test "$(pane_count)" -eq "$before_hkmiss"
assert "hooks 檔缺失：不留 registry" test ! -e "$DHKMISS/agents/hkm.json"

# 16a6-4：hooks 檔是目錄 → 同樣 die、無孤兒（理由同 brief 預檢：`[[ -r ]]` 對
# 目錄一樣成立，只驗 -f 才擋得住）
DHKDIR="$TESTROOT/dhkdir"
mkdir -p "$DHKDIR/agents" "$DHKDIR/locks" "$DHKDIR/tasks" "$TESTROOT/hooks-as-dir"
before_hkdir="$(pane_count)"
env AGENT_BRIDGE_DATA="$DHKDIR" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_CLAUDE_HOOKS="$TESTROOT/hooks-as-dir" \
  "$BRIDGE" spawn hkd --runtime claude >/dev/null 2>"$TESTROOT/hkdir.err"; rc=$?
assert "hooks 檔是目錄：非零退出" test "$rc" -ne 0
assert "hooks 檔是目錄：訊息指出不是普通檔案" \
  grep -q '普通檔案' "$TESTROOT/hkdir.err"
assert "hooks 檔是目錄：不建 pane" test "$(pane_count)" -eq "$before_hkdir"
assert "hooks 檔是目錄：不留 registry" test ! -e "$DHKDIR/agents/hkd.json"

# 16a6-5：hooks 檔缺失＋runtime=codex → spawn 仍成功（codex 不吃 --settings，
# 不該被這份 claude 專屬檔案的缺失影響）
env AGENT_BRIDGE_DATA="$DHKMISS" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_CLAUDE_HOOKS="$TESTROOT/no-such-hooks.json" \
  "$BRIDGE" spawn hkc --runtime codex >/dev/null 2>&1; rc=$?
assert "hooks 檔缺失＋runtime=codex：spawn 仍成功" test "$rc" -eq 0
ab "$DHKMISS" despawn hkc >/dev/null 2>&1 || true

# 16a6-6：share/claude-worker-hooks.json 本身合法且三事件的 command 皆為裸
# `agent-bridge hook ...`——鎖住「不得寫死絕對路徑」這條不變量：PATH 上的
# 裸指令，換機器／換 repo 位置都不用改這份檔
HOOKS_JSON="$ROOT/share/claude-worker-hooks.json"
assert "claude-worker-hooks.json 是合法 JSON" jq -e . "$HOOKS_JSON"
assert "hooks 檔只含 hooks 一個 top-level key" \
  jq -e '(keys | length) == 1 and (keys[0] == "hooks")' "$HOOKS_JSON"
assert "Stop 對應裸指令 agent-bridge hook stop（無路徑寫死）" \
  jq -e '.hooks.Stop[0].hooks[0].command == "agent-bridge hook stop" and .hooks.Stop[0].hooks[0].type == "command"' \
  "$HOOKS_JSON"
assert "UserPromptSubmit 對應裸指令 agent-bridge hook prompt-submit" \
  jq -e '.hooks.UserPromptSubmit[0].hooks[0].command == "agent-bridge hook prompt-submit" and .hooks.UserPromptSubmit[0].hooks[0].type == "command"' \
  "$HOOKS_JSON"
assert "Notification 對應裸指令 agent-bridge hook notification" \
  jq -e '.hooks.Notification[0].hooks[0].command == "agent-bridge hook notification" and .hooks.Notification[0].hooks[0].type == "command"' \
  "$HOOKS_JSON"

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
# spec: CLI-READY-1 ENV-READY-1 ENV-READY-2
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
# spec: CLI-DESPAWN-1
assert_fails "despawn 人工 agent 被拒" ab "$DSPAWN" despawn manual-x
assert "despawn 被拒：registry 檔仍在" test -f "$DSPAWN/agents/manual-x.json"
ab "$DSPAWN" despawn w1 2>/dev/null; rc=$?
assert "despawn spawned agent：exit 0" test "$rc" -eq 0
assert_fails "despawn 後 pane 消失" pane_alive "$pane_w1"
assert "despawn 後 registry 檔已刪" test ! -e "$DSPAWN/agents/w1.json"
# 沒宣告過 disposable＝仍被視為有殘值，直接 despawn 等於繞過收尾流程。
# 機制上不擋，但審計要看得出這次回收沒有筆記（見 cmd_despawn 的 ev 判定）
assert "agents.log 記 despawned-unsaved w1（未宣告 disposable，繞過了收尾）" \
  grep -qE "Z despawned-unsaved w1 ${pane_w1} codex -\$" "$DSPAWN/agents.log"
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
# spec: CLI-DESPAWN-2 ENV-TAG-1
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
# spec: CLI-DESPAWN-3
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
# spec: CLI-SPAWN-2 CLI-DESPAWN-2 STATE-TASK-5

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
# agents.log 是目錄 → 審計失敗觸發回滾；回滾先殺 pane（if-shell）再刪
# registry，shim 就趁 if-shell 那一刻把 agents/ 轉唯讀，讓後續的 rm 必然失敗。
# 殘留 registry 會繼續佔 cap，必須明講而非吞掉。
# 注入點掛在 tmux 而非 date（M3 改）：date 只是 bash 實作恰好會呼叫的外部
# 指令，Rust 實作內建時戳、不 fork date，掛在那裡的話這個場景根本不會發生
# ——測到的是「實作有沒有用 date」而不是「回滾會不會靜默」。if-shell 是兩邊
# 都必經的回滾步驟，換過去之後這條斷言對兩個實作都成立。
DRES="$TESTROOT/dres"
mkdir -p "$DRES/agents" "$DRES/agents.log" "$DRES/locks" "$DRES/tasks"
RESSHIM="$TESTROOT/resshim"
mkdir -p "$RESSHIM"
cat > "$RESSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
# 回滾的第一步是 if-shell（原子驗 tag 才殺）；此時 registry 已落地，
# 鎖住父目錄讓緊接著的 rm 必然失敗
if [[ "\$1" == "if-shell" ]]; then chmod 0555 "$DRES/agents"; fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$RESSHIM/tmux"
ln -s "$BRIDGE" "$RESSHIM/agent-bridge"
ln -s "$SHIM/codex" "$RESSHIM/codex"
before_res="$(pane_count)"
env AGENT_BRIDGE_DATA="$DRES" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$RESSHIM:$PATH" \
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
# spec: STATE-AGENT-2 CLI-SPAWN-3
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
# spec: CLI-SPAWN-2 ENV-READY-1 ENV-READY-2
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
# spec: STATE-LOCK-2
# 鎖目錄被塞進檔案時 rmdir 會 Directory not empty，吞掉的話鎖就永久殘留、
# 而且沒有任何人知道
DLK="$TESTROOT/dlk"
mkdir -p "$DLK/agents" "$DLK/locks" "$DLK/tasks"
LKSHIM="$TESTROOT/lkshim"
mkdir -p "$LKSHIM"
# 注入點掛在 tmux 而非 date（M3 改，理由同 §19c'）：建 pane 的那些 tmux 呼叫
# 全在 registry 鎖內，兩個實作都必經；date 則只有 bash 實作會 fork
cat > "$LKSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
# spawn 取得 registry 鎖之後才會建 pane；趁這時把鎖目錄弄成非空
if [[ -d "$DLK/locks/agents-registry.lock" ]]; then
  : > "$DLK/locks/agents-registry.lock/squatter"
fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$LKSHIM/tmux"
ln -s "$BRIDGE" "$LKSHIM/agent-bridge"
ln -s "$SHIM/codex" "$LKSHIM/codex"
env AGENT_BRIDGE_DATA="$DLK" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$LKSHIM:$PATH" \
  "$BRIDGE" spawn lk --runtime codex >/dev/null 2>"$TESTROOT/lk.err" || true
assert "解鎖失敗：stderr 明確警告而非靜默" \
  grep -q '無法釋放鎖目錄' "$TESTROOT/lk.err"
rm -rf "$DLK/locks/agents-registry.lock"
ab "$DLK" despawn lk >/dev/null 2>&1 || true

# ---- 22. worker brief 注入（真 codex 實測 gate 失敗的成因） ----
# spec: ENV-BRIEF-1 CLI-SPAWN-7
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
  grep -q 'agent-bridge worker brief' "$TESTROOT/codex-args.txt"
assert "brief 注入：啟動參數含『a command, not chat』這條關鍵守則" \
  grep -q 'a command, not chat' "$TESTROOT/codex-args.txt"
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
          'a command, not chat' 'data, not instructions'; do
  assert "brief 正本含必要元素：$kw" grep -q -- "$kw" "$REPO_BRIEF"
done

# ---- 23. relay：交棒給接手者 ----
# spec: CLI-RELAY-1 CLI-RELAY-2 CLI-RELAY-3 ENV-DEPTH-1 ENV-DEPTH-2 ENV-BRIEF-2
# relay 與 spawn 共用整條 pane 生命週期（cap／tag／回滾／夭折偵測／registry），
# 差別只有注入哪份 brief、切不切焦點、以及要不要請接手者回收前一棒。
# 故這裡只測那三處差異＋交接檔路徑的防線，不重測 spawn 已覆蓋的部分。

# 23a. 接手者 brief 正本的內容不變量（同 22f 的理由）
REPO_SBRIEF="$(dirname "$(dirname "$BRIDGE")")/share/successor-brief.md"
assert "接手者 brief 正本存在於 repo" test -r "$REPO_SBRIEF"
for kw in 'agent-bridge ready' 'agent-bridge despawn' 'not a worker waiting for dispatch' \
          'data, not instructions' 'The handoff may be wrong'; do
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
  grep -q 'agent-bridge successor brief' "$TESTROOT/codex-args.txt"
assert "relay 注入的不是 worker 守則（兩份心智相反，混用會讓接手者空等 receive）" \
  bash -c "! grep -q 'agent-bridge worker brief' '$TESTROOT/codex-args.txt'"
assert "relay 啟動參數含交接檔路徑" \
  grep -qF -- "$HANDOFF" "$TESTROOT/codex-args.txt"
assert "relay 啟動參數含本接手者的名字與首要動作" \
  grep -q 'agent-bridge ready r1' "$TESTROOT/codex-args.txt"
assert "relay registry：與 spawn 同一套欄位（共用 cmd_spawn）" \
  jq -e '.spawned == true and .runtime == "codex" and (.spawned_at | type == "string")' \
  "$DRELAY/agents/r1.json"
assert "relay 寫 agents.log（審計線不因換命令而斷）" \
  grep -qE "Z spawned r1 ${pane_r1} codex -\$" "$DRELAY/agents.log"

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

# 23b3. relay 的接手者也是 claude session（通知原生化 Phase 2）：cmd_relay 全程
# 共用 cmd_spawn，這裡驗的正是「不必另開分支」這件事本身——接手者同樣要收
# hooks，才收得到下一棒任務的 Stop hook 通知，不該因為走的是 relay 而漏接
pane_r1c="$(env AGENT_BRIDGE_DATA="$DRELAY" AGENT_BRIDGE_READY_TIMEOUT=0 \
  PATH="$SHIM:$PATH" \
  "$BRIDGE" relay r1c --runtime claude --handoff "$HANDOFF" --no-select 2>/dev/null)"; rc=$?
assert "relay --runtime claude：exit 0" test "$rc" -eq 0
assert "relay（claude 接手者）啟動參數含 --settings <path>（與 worker 共用同一份 hooks）" \
  grep -qF -- "--settings $ROOT/share/claude-worker-hooks.json" "$TESTROOT/claude-args.txt"
r1c_cmd="$(tmx display -pt "$pane_r1c" '#{pane_start_command}')"
assert "relay（claude 接手者）pane_start_command 同樣含 --settings（非僅 argv 檔佐證）" \
  bash -c "c=${r1c_cmd@Q}; [[ \"\$c\" == *'--settings '* ]]"
assert "relay（claude）接手者可正常 despawn（不佔用後續 cap）" \
  env AGENT_BRIDGE_DATA="$DRELAY" PATH="$SHIM:$PATH" "$BRIDGE" despawn r1c

# 未指定 --self-exit 時不該憑空冒出回收指示。
# 不能拿 'agent-bridge despawn' 當關鍵字——接手者 brief 內文本來就有那一段
# （說明被拒是正常的），會恆綠。要鎖的是動態尾巴本身
assert "relay 未指定 --self-exit：prompt 不含回收前一棒的尾巴" \
  bash -c "! grep -q 'After taking over, reclaim your predecessor' '$TESTROOT/codex-args.txt'"
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
  grep -q 'reclaim your predecessor: run agent-bridge despawn prev-agent' \
  "$TESTROOT/codex-args.txt"
assert "relay --self-exit：prompt 說明人工 pane 被拒是正常的（防接手者硬繞）" \
  grep -q 'manually started session the command is refused' "$TESTROOT/codex-args.txt"
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

# 23h. 接力鏈深度上限。接手者守則明文鼓勵「context 吃緊就再交棒」，沒有上界就是
# 無界遞迴——無人值守時一路接下去，燒掉的額度沒有天花板。深度靠
# AGENT_BRIDGE_RELAY_DEPTH 逐棒下傳，人工起的第一棒沒有這個變數＝深度 0。
# 獨立資料目錄＋放大 MAX_SPAWN：本段連開數個接手者，不想撞到與本主題無關的 cap。
DDEPTH="$TESTROOT/ddepth"
env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_MAX_SPAWN=20 \
  "$BRIDGE" relay rd1 --runtime codex --handoff "$HANDOFF" --no-select >/dev/null 2>&1; rc=$?
assert "relay 首棒（呼叫端未設深度）：exit 0" test "$rc" -eq 0
rd1_cmd="$(tmx display -pt "$(jq -r .pane_id "$DDEPTH/agents/rd1.json")" '#{pane_start_command}')"
assert "首棒下傳深度 1，且深度是 exec 前最後一個 env" \
  bash -c "c=${rd1_cmd@Q}; [[ \"\$c\" == *' AGENT_BRIDGE_RELAY_DEPTH=1 exec '* ]]"

env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_MAX_SPAWN=20 AGENT_BRIDGE_RELAY_DEPTH=3 \
  "$BRIDGE" relay rd2 --runtime codex --handoff "$HANDOFF" --no-select >/dev/null 2>&1
rd2_cmd="$(tmx display -pt "$(jq -r .pane_id "$DDEPTH/agents/rd2.json")" '#{pane_start_command}')"
assert "深度逐棒遞增（3 → 4）" \
  bash -c "c=${rd2_cmd@Q}; [[ \"\$c\" == *' AGENT_BRIDGE_RELAY_DEPTH=4 exec '* ]]"

# 達預設上限（10）：必須在建 pane 之前擋下，否則留一個佔 cap 的孤兒
before_d="$(pane_count)"
env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_MAX_SPAWN=20 AGENT_BRIDGE_RELAY_DEPTH=10 \
  "$BRIDGE" relay rd3 --runtime codex --handoff "$HANDOFF" --no-select \
  >/dev/null 2>"$TESTROOT/rd.err"; rc=$?
assert "達接力上限：非零退出" test "$rc" -ne 0
assert "達接力上限：訊息點明需要人介入" grep -q '人介入' "$TESTROOT/rd.err"
assert "達接力上限：不建 pane" test "$(pane_count)" -eq "$before_d"
assert "達接力上限：不留 registry" test ! -e "$DDEPTH/agents/rd3.json"

assert_fails "自訂上限同樣生效（MAX=2、已在第 2 棒）" \
  env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
      AGENT_BRIDGE_MAX_SPAWN=20 AGENT_BRIDGE_RELAY_DEPTH=2 AGENT_BRIDGE_MAX_RELAY_DEPTH=2 \
    "$BRIDGE" relay rd4 --runtime codex --handoff "$HANDOFF" --no-select

# 0 ＝ 解除限制（逃生門：人確認過這條鏈該繼續）
env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_MAX_SPAWN=20 AGENT_BRIDGE_RELAY_DEPTH=99 AGENT_BRIDGE_MAX_RELAY_DEPTH=0 \
  "$BRIDGE" relay rd5 --runtime codex --handoff "$HANDOFF" --no-select >/dev/null 2>&1; rc=$?
assert "MAX_RELAY_DEPTH=0：不設限，深度 99 仍可交棒" test "$rc" -eq 0
rd5_cmd="$(tmx display -pt "$(jq -r .pane_id "$DDEPTH/agents/rd5.json")" '#{pane_start_command}')"
assert "解除限制時深度仍照常遞增（99 → 100）" \
  bash -c "c=${rd5_cmd@Q}; [[ \"\$c\" == *' AGENT_BRIDGE_RELAY_DEPTH=100 exec '* ]]"

assert_fails "深度非數值被拒（不默默當 0 放行）" \
  env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
      AGENT_BRIDGE_MAX_SPAWN=20 AGENT_BRIDGE_RELAY_DEPTH=abc \
    "$BRIDGE" relay rd6 --runtime codex --handoff "$HANDOFF" --no-select
assert_fails "上限非數值被拒" \
  env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
      AGENT_BRIDGE_MAX_SPAWN=20 AGENT_BRIDGE_MAX_RELAY_DEPTH=x \
    "$BRIDGE" relay rd7 --runtime codex --handoff "$HANDOFF" --no-select

# 邊界：空字串必須與非數值同樣 fail-closed。`${VAR:-default}` 會把「已設但為空」
# 吃成預設值，深度於是被靜默重置為 0、cap 形同虛設——獨立複核（2026-07-25）以
# 對抗重跑抓到這條，故實作改用 `${VAR-default}`，這裡鎖住行為不回退
assert_fails "深度為空字串被拒（不得靜默重置成 0）" \
  env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
      AGENT_BRIDGE_MAX_SPAWN=20 AGENT_BRIDGE_RELAY_DEPTH= \
    "$BRIDGE" relay rd8 --runtime codex --handoff "$HANDOFF" --no-select
assert "深度空字串被拒：不留 registry" test ! -e "$DDEPTH/agents/rd8.json"
assert_fails "上限為空字串被拒（不得靜默回到預設 10）" \
  env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
      AGENT_BRIDGE_MAX_SPAWN=20 AGENT_BRIDGE_MAX_RELAY_DEPTH= \
    "$BRIDGE" relay rd9 --runtime codex --handoff "$HANDOFF" --no-select
assert_fails "深度為負數被拒" \
  env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
      AGENT_BRIDGE_MAX_SPAWN=20 AGENT_BRIDGE_RELAY_DEPTH=-1 \
    "$BRIDGE" relay rd10 --runtime codex --handoff "$HANDOFF" --no-select
assert_fails "深度超出 9 位數被拒（格式上限，不是溢位後才發現）" \
  env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
      AGENT_BRIDGE_MAX_SPAWN=20 AGENT_BRIDGE_RELAY_DEPTH=1000000000 \
    "$BRIDGE" relay rd11 --runtime codex --handoff "$HANDOFF" --no-select
# 前導零合法（與 AGENT_BRIDGE_POLL_INTERVAL 等既有數值參數同慣例），且要以
# 十進位解析——沒有 10# 前綴的話 008/009 會被當八進位而報錯
env AGENT_BRIDGE_DATA="$DDEPTH" AGENT_BRIDGE_READY_TIMEOUT=0 PATH="$SHIM:$PATH" \
    AGENT_BRIDGE_MAX_SPAWN=20 AGENT_BRIDGE_RELAY_DEPTH=008 \
  "$BRIDGE" relay rd12 --runtime codex --handoff "$HANDOFF" --no-select >/dev/null 2>&1; rc=$?
assert "前導零深度合法且以十進位解析：exit 0" test "$rc" -eq 0
rd12_cmd="$(tmx display -pt "$(jq -r .pane_id "$DDEPTH/agents/rd12.json")" '#{pane_start_command}')"
assert "前導零 008 解析為 8，下傳 9（非八進位）" \
  bash -c "c=${rd12_cmd@Q}; [[ \"\$c\" == *' AGENT_BRIDGE_RELAY_DEPTH=9 exec '* ]]"

# 收乾淨：pane 是跨測試段共享的資源（registry 各自獨立，pane 不是），本段開的
# 接手者若留著，後面 idle／evict 那些以 pane 佈局為前提的段落會被推歪
for rdn in rd1 rd2 rd5 rd12; do
  assert "23h 收尾：despawn $rdn（pane 還回共享池）" ab "$DDEPTH" despawn "$rdn"
done

# ---- 24. disposable：worker 自報脈絡無殘值（orchestrator Phase 1） ----
# spec: CLI-DISPOSABLE-1
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
# spec: CLI-IDLE-1 CLI-IDLE-2
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
# spec: CLI-EVICT-1 CLI-EVICT-2 CLI-EVICT-3
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
  grep -q 'Wrap-up task' "$DEV/tasks/$tid_ev1/request.md"
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
# spec: CLI-SPAWN-7
# 兩份 brief 是策略層的正本，機制對它們一無所知，所以只有這裡守得住：
# 條款被刪或被改回舊語意時要在測試就紅，而不是等某個 worker 真的把自己
# /clear 掉、脈絡連同 pane 一起消失才發現

# 27a. worker-brief：新語意的正面斷言
for kw in 'agent-bridge disposable' 'context is an asset' 'When a wrap-up task arrives' \
          'Whether to delegate further down to subagents' 'exactly what I may be asked about later'; do
  assert "worker brief 含 Phase 4 條款：$kw" grep -q -- "$kw" "$REPO_BRIEF"
done

# 27b. worker-brief：「reply 後清 context」禁令必須在。
# 「reply 後清空 context」與「保留脈絡供追問」直接矛盾——照做的話 reply 完
# 脈絡就沒了，disposable 宣告與 evict 的收尾筆記全部失去意義。英譯後改守
# 正面禁令字句（原中文舊條款的否定式 grep 對英文正文已是空轉）
assert "worker brief 保留「不要清自己的 context」禁令" \
  grep -qF 'Do not clear your own context' "$REPO_BRIEF"
# shellcheck disable=SC2016  # 反引號是 Markdown 字面值，-F 比對、無展開需求
assert "worker brief 保留 no-/clear 待命條款" \
  grep -qF 'No `/clear`' "$REPO_BRIEF"

# 27c. orchestrator-brief：存在 + 策略要點
REPO_OBRIEF="$(dirname "$(dirname "$BRIDGE")")/share/orchestrator-brief.md"
assert "orchestrator brief 正本存在於 repo" test -r "$REPO_OBRIEF"
for kw in 'agent-bridge idle' 'agent-bridge evict' 'evicted-timeout' \
          'Keep by default' 'AGENT_BRIDGE_MAX_SPAWN' \
          'Do not call the AgentTool' \
          'untrusted external' 'Route by blast radius'; do
  assert "orchestrator brief 含必要元素：$kw" grep -q -- "$kw" "$REPO_OBRIEF"
done

# ---- 28. gc：清舊 task，但三道保留線都不能破 ----
# spec: CLI-GC-1 CLI-GC-2 CLI-GC-3 STATE-TASK-5
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
# spec: CLI-GC-3 CLI-EVICT-3
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

# ---- 30. CC 權限框特徵 canary：安裝中的 claude 仍含防線依賴的字串 ----
# spec: HOOK-NOTIFY-2
# screen_has_prompt 的特徵是對 Claude Code UI 文案的字串比對（bin 內註明以
# 2.1.218 可執行檔 strings 佐證）。CC 改版改文案時，防線靜默退回 fail-open
# （通知的 Enter 誤觸權限框會重現），沒有任何訊號——這組 canary 把「靜默
# 失效」變成「測試紅燈」。比對目標是 readlink 解析後的實體執行檔（本機為
# ELF，字串內嵌）；找不到 claude 時跳過不失敗，維持套件其餘部分零外部依賴。
# 特徵字面值必須與 bin/agent-bridge 的 screen_has_prompt 保持逐字一致。
if [[ -n "$REAL_CLAUDE" ]] && CANARY_REAL="$(readlink -f "$REAL_CLAUDE" 2>/dev/null)" \
   && [[ -f "$CANARY_REAL" ]]; then
  for kw in 'Do you want to ' 'Esc to cancel' \
            'has written up a plan' 'Would you like to proceed'; do
    assert "CC canary：claude 執行檔仍含特徵「$kw」" \
      grep -qaF -- "$kw" "$CANARY_REAL"
  done
  # 特徵同步斷言：canary 驗的字串必須真的還是 bin 在用的字串，
  # 否則 bin 改了特徵、canary 還在守舊字串，兩邊各測各的。
  # 比對範圍限 screen_has_prompt 函式本體：前三組字串在函式外的註解也
  # 出現，搜整檔會讓「函式改了 matcher、註解留著舊字串」照樣誤綠
  # （獨立複核 2026-07-24 指出）。函式被改名或抽不出來時檔案為空，
  # 後續 grep 全紅——失效方向是誤報而非漏報
  # 抽取對象是**源碼正本**而非 $BRIDGE（M3 改、M4 改綁 Rust）：這是源碼耦合
  # 檢查，與 check-contract 1–2 同一類，不是黑箱行為斷言。$BRIDGE 是編譯後的
  # 執行檔，sed 抽不出函式本體，硬綁它只會退化成「換實作就全紅」。
  # 抽出後**剝掉註解行**再比對：光 grep -F 的話，把 matcher 換成新字串、
  # 同時在函式內留一段含舊字串的註解，四個斷言仍全綠（獨立複核 2026-07-31）。
  # 剝註解把蒙混面壓到「函式內未使用的字面值／死分支」，那層由
  # notify.rs 的 matcher_uses_the_canary_feature_strings 單元測試鎖
  if [[ "$SRC_KIND" == bash ]]; then
    sed -n '/^screen_has_prompt() {/,/^}/p' "$SRC_BASH" \
      | grep -v '^[[:space:]]*#' > "$TESTROOT/canary-fn"
  else
    sed -n '/^pub fn screen_has_prompt/,/^}/p' "$SRC_NOTIFY_RS" \
      | grep -v '^[[:space:]]*//' > "$TESTROOT/canary-fn"
  fi
  assert "CC canary：screen_has_prompt 函式本體可抽出" test -s "$TESTROOT/canary-fn"
  for kw in 'Do you want to ' 'Esc to cancel' \
            'has written up a plan' 'Would you like to proceed'; do
    assert "CC canary：特徵「$kw」與正本 screen_has_prompt 一致" \
      grep -qF -- "$kw" "$TESTROOT/canary-fn"
  done
else
  printf 'SKIP: CC canary（無可檢查的 claude 執行檔——PATH 找不到、readlink 失敗或非一般檔案；特徵漂移檢查未執行）\n'
fi

# ---- 31. 第三輪獨立複核的修補（2026-07-24）----
# spec: CLI-STATUS-1 CLI-READ-1 CLI-AWAIT-2 CLI-DESPAWN-3 CLI-EVICT-2 CLI-RO-1 ENV-POLL-1 CLI-GEN-3 CLI-SEND-3 STATE-TASK-4 STATE-TASK-5
# 複核輪抓出的缺口，各自鎖一個回歸；編號對應複核報告的 finding

D31="$TESTROOT/d31"
mkdir -p "$D31/tasks/20200101T000000Z-aaaa"
printf 'queued\n' > "$D31/tasks/20200101T000000Z-aaaa/status"

# 31a. F5：task-id 拒 `.`／`..`／dotfile／旗標形。`.`/`..` 會把
# "$TASKS_DIR/$id" alias 到 tasks/ 與整個資料目錄且 [[ -d ]] 成立，
# 修補前 `status .` 甚至以 rc=0 蒙混
assert_fails "task-id '.' 被拒（曾 rc=0 蒙混）" ab "$D31" status .
# 拒因要鎖在 regex 本身：光驗非零的話，regex 回退後 `.` 仍會因 alias 目錄
# 缺 status 檔而非零，測試誤綠（第二輪複核指出）
ab "$D31" status . 2> "$TESTROOT/err-31a" || true
assert "task-id '.' 拒因是 task-id 不合法（鎖 regex 非碰巧讀不到）" \
  grep -qF 'task-id 不合法' "$TESTROOT/err-31a"
assert_fails "task-id '..' 被拒" ab "$D31" status ..
assert_fails "task-id '.hidden' 被拒（dotfile 形）" ab "$D31" status .hidden
assert_fails "task-id '-x' 被拒（旗標形）" ab "$D31" status -x

# 31b. F5：缺 status 檔的損壞 task，查詢必須非零（修補前 rc=0＋空輸出，
# 監控會當成功）
mkdir -p "$D31/tasks/20200101T000000Z-bbbb"
assert_fails "缺 status 檔的 status 查詢非零" ab "$D31" status 20200101T000000Z-bbbb

# 31c. F1：await 的輪詢間隔進迴圈前就驗；真逾時專用 exit 124，
# 操作性失敗不得偽裝成逾時
rc=0
env AGENT_BRIDGE_POLL_INTERVAL=bad AGENT_BRIDGE_DATA="$D31" PATH="$SHIM:$PATH" \
  "$BRIDGE" await 20200101T000000Z-aaaa --timeout 300 >/dev/null 2>&1 || rc=$?
assert "壞 POLL_INTERVAL：await 非零退出" test "$rc" -ne 0
assert "壞 POLL_INTERVAL：rc 不是 124（不偽裝成逾時）" test "$rc" -ne 124
# 拒因要鎖在進迴圈前的預先驗證：沒有驗證時 sleep 自己也會非零、非 124，
# 上兩個斷言照樣綠（第二輪複核指出）
env AGENT_BRIDGE_POLL_INTERVAL=bad AGENT_BRIDGE_DATA="$D31" PATH="$SHIM:$PATH" \
  "$BRIDGE" await 20200101T000000Z-aaaa --timeout 300 2> "$TESTROOT/err-31c" || true
assert "壞 POLL_INTERVAL 拒因是預先驗證（訊息含變數名）" \
  grep -qF 'AGENT_BRIDGE_POLL_INTERVAL' "$TESTROOT/err-31c"
rc=0
env AGENT_BRIDGE_POLL_INTERVAL=0.1 AGENT_BRIDGE_DATA="$D31" PATH="$SHIM:$PATH" \
  "$BRIDGE" await 20200101T000000Z-aaaa --timeout 1 >/dev/null 2>&1 || rc=$?
assert "await 真逾時 exit 124" test "$rc" -eq 124

# 31d. F1：evict 遇 await 操作性失敗必須中止並保留 worker——修補前空字串
# 一律落 evicted-timeout，10ms 就殺掉活的 worker、審計還說是逾時
mkdir -p "$D31/agents"
jq -n '{name:"ev31", pane_id:"%999", registered_at:"2026-01-01T00:00:00Z",
        spawned:true, runtime:"codex", model:"", spawned_at:"2026-01-01T00:00:00Z",
        ready:true, spawn_tag:"AGENT_BRIDGE_SPAWN_TAG=ab-spawn-ev31-1-0123456789ab"}' \
  > "$D31/agents/ev31.json"
rc=0
env AGENT_BRIDGE_POLL_INTERVAL=bad AGENT_BRIDGE_DATA="$D31" PATH="$SHIM:$PATH" \
  "$BRIDGE" evict ev31 --timeout 300 >/dev/null 2>&1 || rc=$?
assert "evict 遇 await 操作性失敗：非零中止" test "$rc" -ne 0
assert "evict 中止：worker registry 未動" test -f "$D31/agents/ev31.json"
assert_fails "evict 中止：無 evicted* 審計" evt_grep "$D31/agents.log" 'evicted[a-z-]*'

# 31e. F2：審計不可寫時 despawn 拒絕動手（agents.log 換成目錄讓 append 必敗）；
# 修補前 kill＋刪 registry 之後 append 才失敗，非零收場誤導呼叫端重試
D31B="$TESTROOT/d31b"
mkdir -p "$D31B/agents" "$D31B/agents.log"
jq -n '{name:"aud31", pane_id:"%999", registered_at:"2026-01-01T00:00:00Z",
        spawned:true, runtime:"codex", model:"", spawned_at:"2026-01-01T00:00:00Z",
        ready:true, spawn_tag:"AGENT_BRIDGE_SPAWN_TAG=ab-spawn-aud31-1-0123456789ab"}' \
  > "$D31B/agents/aud31.json"
rc=0; ab "$D31B" despawn aud31 >/dev/null 2>&1 || rc=$?
assert "audit 不可寫：despawn 拒絕動手（非零）" test "$rc" -ne 0
assert "audit 不可寫：registry 未動" test -f "$D31B/agents/aud31.json"

# 31f. F7：send 的訊息來源檔不存在→先驗先死，不留殘缺 task 目錄
# （gc 只清完整形狀，殘缺目錄是永久孤兒）
D31C="$TESTROOT/d31c"
mkdir -p "$D31C/agents"
jq -n '{name:"w31", pane_id:"%998", registered_at:"2026-01-01T00:00:00Z"}' \
  > "$D31C/agents/w31.json"
assert_fails "send 來源檔不存在被拒" \
  ab "$D31C" send w31 --from me --message-file "$TESTROOT/no-such-file"
assert "send 被拒後 tasks/ 零殘留" \
  test -z "$(ls -A "$D31C/tasks" 2>/dev/null)"

# 31g. F6：read 取 task 鎖，與 gc --apply 互斥——鎖被佔用時不硬讀
# （模擬 gc 正持鎖刪目錄；約 5 秒重試後失敗是預期成本）
D31D="$TESTROOT/d31d"
mkdir -p "$D31D/tasks/20200101T000000Z-cccc" "$D31D/locks/20200101T000000Z-cccc.lock"
printf 'completed\n' > "$D31D/tasks/20200101T000000Z-cccc/status"
jq -n '{from:"a", to:"b"}' > "$D31D/tasks/20200101T000000Z-cccc/metadata.json"
printf 'hi\n' > "$D31D/tasks/20200101T000000Z-cccc/response.md"
assert_fails "read 在 task 鎖被佔用時不硬讀" ab "$D31D" read 20200101T000000Z-cccc
rmdir "$D31D/locks/20200101T000000Z-cccc.lock"
assert "釋鎖後 read 正常" ab "$D31D" read 20200101T000000Z-cccc

# 31h. F8：唯讀查詢（status/await/idle/list）不建資料目錄——「唯讀」宣稱
# 落到實作，嚴格只讀 sandbox 下查詢不再無中生有
D31E="$TESTROOT/d31e"
ab "$D31E" idle >/dev/null 2>&1 || true
ab "$D31E" list >/dev/null 2>&1 || true
ab "$D31E" status 20200101T000000Z-dddd >/dev/null 2>&1 || true
ab "$D31E" await 20200101T000000Z-dddd --timeout 1 >/dev/null 2>&1 || true
assert "唯讀查詢不建資料目錄" test ! -d "$D31E"

# 31i. F3：update_meta_status 先寫裸 status 再寫 metadata（順序即修補本體：
# 反向殘留的是「終態轉換可被重放」的 split-brain 方向）。源碼順序不變量，
# 比照 §30 的函式本體抽取
UMS_FN="$TESTROOT/ums-fn"
# 抽取對象是源碼正本，理由同 §30 的 canary-fn（源碼耦合檢查，M4 已改綁 Rust）。
# 註解行同樣先剝掉
if [[ "$SRC_KIND" == bash ]]; then
  sed -n '/^update_meta_status() {/,/^}/p' "$SRC_BASH" \
    | grep -v '^[[:space:]]*#' > "$UMS_FN"
  # shellcheck disable=SC2016  # $dir 是源碼字面值，刻意不展開
  SL_PAT='atomic_write "$dir/status"'
  # shellcheck disable=SC2016  # 同上
  ML_PAT='atomic_write "$dir/metadata.json"'
else
  sed -n '/^pub fn update_meta_status/,/^}/p' "$SRC_TASK_RS" \
    | grep -v '^[[:space:]]*//' > "$UMS_FN"
  # 各鎖**完整的呼叫頭＋第一引數**，不是單抽路徑字面值：後者可被
  # `let p = dir.join("status");` 這種 decoy 先命中，實際寫入順序反轉了斷言
  # 照樣綠（獨立複核 2026-07-31 給的反例）
  SL_PAT='atomic_write(&dir.join("status")'
  ML_PAT='atomic_write(&meta_path'
fi
assert "update_meta_status 函式本體可抽出" test -s "$UMS_FN"
# 比對前把空白全部去掉：呼叫跨幾行是 rustfmt 的事，不該讓它決定斷言成敗
UMS_FLAT="$(tr -d '[:space:]' < "$UMS_FN")"
sl="$(awk -v s="$UMS_FLAT" -v p="${SL_PAT// /}" 'BEGIN{print index(s,p)}')"
ml="$(awk -v s="$UMS_FLAT" -v p="${ML_PAT// /}" 'BEGIN{print index(s,p)}')"
assert "先寫 status 再寫 metadata（split-brain 方向鎖定）" \
  test "$sl" -gt 0 -a "$ml" -gt 0 -a "$sl" -lt "$ml"

# 31j. F1 第二輪：await 迴圈內 status 消失＝操作性失敗。evict 以 `||` 呼叫端
# 包住 cmd_await，該語境抑制 errexit——修補前裸讀取失敗被靜靜輪詢到期限、
# 誤分類成逾時而殺 pane；修補後迴圈內顯式 die，evict 必須中止且不動 worker
D31F="$TESTROOT/d31f"
mkdir -p "$D31F/agents"
jq -n '{name:"ev31f", pane_id:"%999", registered_at:"2026-01-01T00:00:00Z",
        spawned:true, runtime:"codex", model:"", spawned_at:"2026-01-01T00:00:00Z",
        ready:true, spawn_tag:"AGENT_BRIDGE_SPAWN_TAG=ab-spawn-ev31f-1-0123456789ab"}' \
  > "$D31F/agents/ev31f.json"
(
  env AGENT_BRIDGE_POLL_INTERVAL=0.2 AGENT_BRIDGE_DATA="$D31F" PATH="$SHIM:$PATH" \
    "$BRIDGE" evict ev31f --timeout 60 >/dev/null 2>&1
  echo "$?" > "$TESTROOT/evict-rc-31j"
) &
EV31J_PID=$!
# 等收尾任務的 status 檔真的落地再抽走它（等目錄不夠：send 可能還沒寫到 status）
wait_for 10 bash -c "compgen -G '$D31F/tasks/*/status' >/dev/null"
rm -f "$D31F"/tasks/*/status
wait_for 15 test -f "$TESTROOT/evict-rc-31j"
wait "$EV31J_PID" 2>/dev/null || true
assert "evict 遇 await 迴圈內操作失敗：非零中止（不偽裝逾時）" \
  test "$(cat "$TESTROOT/evict-rc-31j" 2>/dev/null || echo 0)" -ne 0
assert "迴圈內失敗：worker registry 未動" test -f "$D31F/agents/ev31f.json"
assert_fails "迴圈內失敗：無 evicted* 審計" evt_grep "$D31F/agents.log" 'evicted[a-z-]*'

# 31k. F7 第二輪：預檢通過、寫入階段才失敗（來源檔不可讀）→ 回滾殘缺目錄。
# 光擋「一開始就不存在」涵蓋不了 TOCTOU 殘徑；此測例以權限製造確定性的
# post-mkdir 失敗，驗 EXIT trap 回滾。（以一般使用者跑測試為前提；root 下
# chmod 000 仍可讀，本測例會誤紅）
BAD31="$TESTROOT/unreadable-msg"
printf 'x\n' > "$BAD31"
chmod 000 "$BAD31"
assert_fails "send 來源檔不可讀：非零" \
  ab "$D31C" send w31 --from me --message-file "$BAD31"
assert "寫入階段失敗仍零殘留（EXIT trap 回滾）" \
  test -z "$(ls -A "$D31C/tasks" 2>/dev/null)"
chmod 600 "$BAD31"

# ---- 32. spawn 落點：per-owner worker window＋owner/actor 審計（2026-07-25）----
# spec: CLI-SPAWN-5 CLI-SPAWN-6
# 前面所有測例都在 tmux 外呼叫 spawn（TMUX 已 unset）→ 舊行為（目前視窗
# split），上面已覆蓋。本節驗「orchestrator 本身在 pane 內」的新路徑：
# worker 落進 owner 的 worker window（不交給 tmux 的 client 焦點解析）、
# 同 owner 第二次 spawn 沿用同一窗、registry 記 owner/worker_window、
# agents.log 尾欄記 actor、--window 專屬視窗不寫 worker_window。
# 測試環境沒有 attached client，無從重現「落到使用者焦點視窗」的原始故障；
# 能驗的是落點已被顯式錨定到 owner。
D32="$TESTROOT/d32"
orc_cmd="$(printf 'env AGENT_BRIDGE_DATA=%q AGENT_BRIDGE_READY_TIMEOUT=1 AGENT_BRIDGE_READY_PROBE_INTERVAL=0.5 PATH=%q bash --norc --noprofile' \
  "$D32" "$SHIM:$PATH")"
tmx new-session -d -s orc -x 200 -y 100 "$orc_cmd"
ORC_PANE="$(tmx list-panes -t orc -F '#{pane_id}')"
ORC_WIN="$(tmx display-message -p -t "$ORC_PANE" '#{window_id}')"
ORC_IDX="$(tmx display-message -p -t "$ORC_PANE" '#{window_index}')"
tmx send-keys -t "$ORC_PANE" "$(printf 'touch %q' "$TESTROOT/orc-ready")" Enter
if ! wait_for 10 test -f "$TESTROOT/orc-ready"; then
  bad "32 前置：orchestrator 假 pane 未就緒"
fi
# -a 錨定的 sentinel：orc window 之後先放一個窗。spawn 未錨定（append）時
# worker 會落在 sentinel 之後，index 斷言才抓得到 mutation
SENT32="$(tmx new-window -dP -t orc: -n sentinel32 -F '#{window_id}')"

# 32a. in-pane spawn：worker 進新開的 worker window，不與 orchestrator 同窗
tmx send-keys -t "$ORC_PANE" \
  "agent-bridge spawn w32a --runtime codex >$TESTROOT/w32a.out 2>/dev/null; echo rc=\$? >$TESTROOT/w32a.done" Enter
wait_for 20 test -f "$TESTROOT/w32a.done"
W32A_PANE="$(cat "$TESTROOT/w32a.out" 2>/dev/null || true)"
# 空 pane 變數不可餵給 display-message：-t '' 會解析到 current window，
# 讓後面的比較在 spawn 失敗時空洞通過
W32A_WIN=""
[[ -n "$W32A_PANE" ]] && W32A_WIN="$(tmx display-message -p -t "$W32A_PANE" '#{window_id}' 2>/dev/null || true)"
assert "32a in-pane spawn：worker pane 存活" pane_alive "$W32A_PANE"
assert "32a worker 不與 orchestrator 同窗" \
  test -n "$W32A_WIN" -a "$W32A_WIN" != "$ORC_WIN"
assert "32a worker 與 orchestrator 同 session" \
  test -n "$W32A_PANE" -a "$(tmx display-message -p -t "${W32A_PANE:-%none}" '#{session_name}' 2>/dev/null)" = "orc"
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
win_name_is_ab() { [[ "$(tmx display-message -p -t "$1" '#{window_name}' 2>/dev/null)" == ab:* ]]; }
assert "32a worker window 名帶 ab: 前綴" win_name_is_ab "$W32A_PANE"
assert "32a worker window 緊鄰 orchestrator window 之後（-a 錨定）" \
  test "$(tmx display-message -p -t "${W32A_PANE:-%none}" '#{window_index}' 2>/dev/null)" = "$(( ORC_IDX + 1 ))"
assert "32a -a 錨定：sentinel 被擠到 worker window 之後" \
  test "$(tmx display-message -p -t "$SENT32" '#{window_index}' 2>/dev/null)" = "$(( ORC_IDX + 2 ))"
assert "32a worker window 開 pane-border-status top" \
  test "$(tmx show-options -wv -t "${W32A_PANE:-%none}" pane-border-status 2>/dev/null)" = "top"
assert "32a pane 標題＝name (runtime)" \
  test "$(tmx display-message -p -t "$W32A_PANE" '#{pane_title}' 2>/dev/null)" = "w32a (codex)"
# shellcheck disable=SC2016  # $o/$w 是 jq 的變數，不是 shell 展開
assert "32a registry 記 owner（session:@window）" \
  jq -e --arg o "orc:$ORC_WIN" '.owner == $o' "$D32/agents/w32a.json"
# shellcheck disable=SC2016
assert "32a registry 記 worker_window" \
  jq -e --arg w "$W32A_WIN" '.worker_window == $w' "$D32/agents/w32a.json"
assert "32a agents.log spawned 尾欄記 actor＝owner" \
  grep -qE "Z spawned w32a [^ ]+ codex orc:@[0-9]+\$" "$D32/agents.log"

# 32b. 同 owner 第二次 spawn：沿用同一個 worker window
tmx send-keys -t "$ORC_PANE" \
  "agent-bridge spawn w32b --runtime codex >$TESTROOT/w32b.out 2>/dev/null; echo rc=\$? >$TESTROOT/w32b.done" Enter
wait_for 20 test -f "$TESTROOT/w32b.done"
W32B_PANE="$(cat "$TESTROOT/w32b.out" 2>/dev/null || true)"
W32B_WIN=""
[[ -n "$W32B_PANE" ]] && W32B_WIN="$(tmx display-message -p -t "$W32B_PANE" '#{window_id}' 2>/dev/null || true)"
assert "32b 第二個 worker 沿用同一 worker window" \
  test -n "$W32B_WIN" -a "$W32B_WIN" = "$W32A_WIN"
# layout 均分不變量：2 pane 高度差 ≤1。實測（tmux 3.7b）tiled 對 2 pane 是
# 上下堆疊（h=49/50），與預設 split 的幾何不可區分——第二輪複核建議的
# pane_left 相異斷言實測不成立。「刪 select-layout」的 mutation 要第三個
# spawn（3 pane 網格 vs 連續對半切）才鎖得住，成本不划算，裁決不鎖
# （同 31e/31g/31h 類）。此斷言保「不出現極端不均的退化排版」這個較弱
# 但真實的不變量
# 差值容忍 2 而非 1：pane-border-status top 每 pane 佔一行、不計入
# pane_height，tiled 均分後實測 48/50（重現腳本 scratchpad repro32）
mapfile -t W32HS < <(tmx list-panes -t "${W32A_WIN:-@none}" -F '#{pane_height}' 2>/dev/null)
W32HDIFF=$(( ${W32HS[0]:-0} - ${W32HS[1]:-0} ))
W32HDIFF=${W32HDIFF#-}
assert "32b worker window 均分（2 pane、高度差 ≤2，含 border-status 行）" \
  test "${#W32HS[@]}" -eq 2 -a "$W32HDIFF" -le 2

# 32c. --window：獨立視窗、不寫 worker_window（不被後續 spawn 撿去共用）
tmx send-keys -t "$ORC_PANE" \
  "agent-bridge spawn w32c --runtime codex --window >$TESTROOT/w32c.out 2>/dev/null; echo rc=\$? >$TESTROOT/w32c.done" Enter
wait_for 20 test -f "$TESTROOT/w32c.done"
W32C_PANE="$(cat "$TESTROOT/w32c.out" 2>/dev/null || true)"
W32C_WIN=""
[[ -n "$W32C_PANE" ]] && W32C_WIN="$(tmx display-message -p -t "$W32C_PANE" '#{window_id}' 2>/dev/null || true)"
assert "32c --window：獨立於 worker window 與 orchestrator 窗" \
  test -n "$W32C_WIN" -a "$W32C_WIN" != "$W32A_WIN" -a "$W32C_WIN" != "$ORC_WIN"
assert "32c --window 不寫 worker_window" \
  jq -e '.worker_window == ""' "$D32/agents/w32c.json"

# 32d. tmux 外 despawn：審計 actor 記 -（既有 tmux 外 spawned 的 - 已在
# 測例 12/13 的行尾錨定斷言覆蓋）
ab "$D32" despawn w32c >/dev/null 2>&1
assert "32d tmux 外 despawn：actor 記 -" \
  grep -qE "Z despawned-unsaved w32c [^ ]+ codex -\$" "$D32/agents.log"

# 32e. 敵意 registry：worker_window 填「語法合法且存在」的 @id，一個指向
# 他 session 的窗、一個指向同 session 但非 ab: 建窗——兩者都不得改寫落點
# （confused-deputy gate：同 session＋ab: 前綴驗證，獨立複核 2026-07-25 指出）
IT_WIN="$(tmx display-message -p -t "$PANE_A" '#{window_id}')"
jq -n --arg o "orc:$ORC_WIN" --arg w "$IT_WIN" \
  '{name:"evil-a",pane_id:"%99",registered_at:"t",spawned:true,runtime:"codex",
    model:"",spawned_at:"t",ready:false,spawn_tag:"x",owner:$o,worker_window:$w}' \
  > "$D32/agents/evil-a.json"
jq -n --arg o "orc:$ORC_WIN" --arg w "$ORC_WIN" \
  '{name:"evil-b",pane_id:"%99",registered_at:"t",spawned:true,runtime:"codex",
    model:"",spawned_at:"t",ready:false,spawn_tag:"x",owner:$o,worker_window:$w}' \
  > "$D32/agents/evil-b.json"
tmx send-keys -t "$ORC_PANE" \
  "AGENT_BRIDGE_MAX_SPAWN=8 agent-bridge spawn w32e --runtime codex >$TESTROOT/w32e.out 2>/dev/null; echo rc=\$? >$TESTROOT/w32e.done" Enter
wait_for 20 test -f "$TESTROOT/w32e.done"
W32E_PANE="$(cat "$TESTROOT/w32e.out" 2>/dev/null || true)"
W32E_WIN=""
[[ -n "$W32E_PANE" ]] && W32E_WIN="$(tmx display-message -p -t "$W32E_PANE" '#{window_id}' 2>/dev/null || true)"
assert "32e 敵意 worker_window（他 session／非 ab: 窗）不改寫落點，仍用合法窗" \
  test -n "$W32E_WIN" -a "$W32E_WIN" != "$IT_WIN" -a "$W32E_WIN" != "$ORC_WIN" -a "$W32E_WIN" = "$W32A_WIN"

# 32g. 第二輪反例：同 session、ab: 名稱、但 @ab_owner 印記屬他 owner 的窗
# ——不得沿用（registry-only 攻擊者可冒 owner／worker_window，冒不了 tmux
# 視窗選項）
EVIL32_WIN="$(tmx new-window -dP -t orc: -n 'ab:stolen' -F '#{window_id}')"
tmx set-option -w -t "$EVIL32_WIN" '@ab_owner' 'orc:@999'
jq -n --arg o "orc:$ORC_WIN" --arg w "$EVIL32_WIN" \
  '{name:"evil-c",pane_id:"%99",registered_at:"t",spawned:true,runtime:"codex",
    model:"",spawned_at:"t",ready:false,spawn_tag:"x",owner:$o,worker_window:$w}' \
  > "$D32/agents/evil-c.json"
tmx send-keys -t "$ORC_PANE" \
  "AGENT_BRIDGE_MAX_SPAWN=8 agent-bridge spawn w32g --runtime codex >$TESTROOT/w32g.out 2>/dev/null; echo rc=\$? >$TESTROOT/w32g.done" Enter
wait_for 20 test -f "$TESTROOT/w32g.done"
W32G_PANE="$(cat "$TESTROOT/w32g.out" 2>/dev/null || true)"
W32G_WIN=""
[[ -n "$W32G_PANE" ]] && W32G_WIN="$(tmx display-message -p -t "$W32G_PANE" '#{window_id}' 2>/dev/null || true)"
assert "32g 他 owner 印記的 ab: 窗不被沿用，仍用自己的窗" \
  test -n "$W32G_WIN" -a "$W32G_WIN" != "$EVIL32_WIN" -a "$W32G_WIN" = "$W32A_WIN"

# 32h. 污染 registry 的 pane/runtime（含空白）流進 disposable 審計——寫入點
# 必須摺疊，欄位安全不靠上游（獨立複核第二輪 8 欄實例）
jq -n \
  '{name:"evil-d",pane_id:"bad pane",registered_at:"t",spawned:true,
    runtime:"co dex",model:"",spawned_at:"t",ready:false,spawn_tag:"x"}' \
  > "$D32/agents/evil-d.json"
assert "32h 污染 registry 的 disposable 仍成功" ab "$D32" disposable evil-d
assert "32h 污染欄位寫入審計前被摺疊" \
  grep -qE "Z disposable evil-d bad_pane co_dex -\$" "$D32/agents.log"

# 32i. 新建窗的 @ab_owner 印記寫入失敗必須翻盤回滾（第三輪複核指出：
# 靜默吞掉會做出 spawn 成功但永不可沿用的窗）。選擇性 shim：只讓帶
# @ab_owner 的 tmux 呼叫失敗，其餘轉真 tmux；用新 owner（orc2 window）
# 觸發「新建」分支——orc 的既有合法窗走的是沿用分支（重寫失敗容忍）
STAMPFAIL="$TESTROOT/stampfail"
mkdir -p "$STAMPFAIL"
# shellcheck disable=SC2016  # $@ 是 shim 腳本的內容，要 literal 不展開
printf '#!/usr/bin/env bash\nfor a in "$@"; do [[ "$a" == "@ab_owner" ]] && exit 1; done\nunset TMUX\nexec %q -L %q -f /dev/null "$@"\n' \
  "$REAL_TMUX" "$SOCK" > "$STAMPFAIL/tmux"
chmod +x "$STAMPFAIL/tmux"
ORC2_PANE="$(tmx new-window -dPF '#{pane_id}' -t orc: "$orc_cmd")"
tmx send-keys -t "$ORC2_PANE" "$(printf 'touch %q' "$TESTROOT/orc2-ready")" Enter
wait_for 10 test -f "$TESTROOT/orc2-ready"
PANES_BEFORE_32I="$(pane_count)"
tmx send-keys -t "$ORC2_PANE" \
  "PATH=$STAMPFAIL:\$PATH AGENT_BRIDGE_MAX_SPAWN=16 agent-bridge spawn w32i --runtime codex >$TESTROOT/w32i.out 2>$TESTROOT/w32i.err; echo rc=\$? >$TESTROOT/w32i.done" Enter
wait_for 20 test -f "$TESTROOT/w32i.done"
assert "32i 印記寫入失敗：spawn 非零收場" \
  bash -c "grep -q 'rc=0' '$TESTROOT/w32i.done' && exit 1 || grep -q 'rc=' '$TESTROOT/w32i.done'"
assert "32i 死因確為印記寫入（非 cap 等他因）" \
  grep -q '@ab_owner' "$TESTROOT/w32i.err"
assert "32i 回滾：registry 無 w32i" bash -c "! test -e '$D32/agents/w32i.json'"
W32I_SURVIVORS="$(tmx list-panes -a -F '#{pane_start_command}' 2>/dev/null | grep -c 'ab-spawn-w32i' || true)"
assert "32i 回滾：無 w32i pane 殘留" test "$W32I_SURVIVORS" -eq 0
assert "32i 回滾：pane 數回到 spawn 前" test "$(pane_count)" -eq "$PANES_BEFORE_32I"

# 32f. 欄位安全不變量：agents.log 每行恰 6 個空白分隔欄（含 32h 的污染注入）
assert "32f agents.log 每行恰 6 欄" \
  bash -c "! grep -qEv '^[^ ]+ [^ ]+ [^ ]+ [^ ]+ [^ ]+ [^ ]+\$' '$D32/agents.log'"

# ---- 33. 通知原生化 Phase 1：hook 子命令＋notify_or_defer（declarative-juggling-lark）----
# spec: CLI-HOOK-1 HOOK-ID-1 HOOK-ID-2 HOOK-EVT-1 HOOK-EVT-2 HOOK-EVT-3 HOOK-EVT-4 HOOK-NOTIFY-1 STATE-CHAN-1 STATE-CHAN-2 STATE-CHAN-3 ENV-TTL-1

# 33.1 hook stop：有 queued task → block JSON＋state busy/last_delivered
D33="$TESTROOT/d33"
ab "$D33" register zoe "$PANE_A" 2>/dev/null
id33_1="$(ab "$D33" send zoe --from alice --message t1 2>/dev/null)"
TAG33="ab-spawn-zoe-12345-0123456789ab"
hookcall "$D33" "$TAG33" stop '{"session_id":"s33"}' > "$TESTROOT/h1.out" 2>"$TESTROOT/h1.err"; rc=$?
assert "33.1 hook stop：exit 0" test "$rc" -eq 0
assert "33.1 hook stop：stdout 是合法 JSON" jq -e . "$TESTROOT/h1.out"
assert "33.1 hook stop：decision=block" \
  bash -c "test \"\$(jq -r .decision '$TESTROOT/h1.out')\" = block"
assert "33.1 hook stop：reason 含 receive <id>" grep -q "receive $id33_1" "$TESTROOT/h1.out"
assert "33.1 hook stop：state 檔 state=busy" state_field_is "$D33/state/zoe.json" state busy
assert "33.1 hook stop：state 檔 last_delivered 正確" \
  state_field_is "$D33/state/zoe.json" last_delivered "$id33_1"

# 33.2 hook stop：stop_hook_active=true＋同 last_delivered id → 無 stdout、放行、state=idle
hookcall "$D33" "$TAG33" stop '{"stop_hook_active":true,"session_id":"s33"}' > "$TESTROOT/h2.out" 2>"$TESTROOT/h2.err"; rc=$?
assert "33.2 hook stop 同 id 放行：exit 0" test "$rc" -eq 0
assert "33.2 hook stop 同 id 放行：無 stdout" test ! -s "$TESTROOT/h2.out"
assert "33.2 hook stop 同 id 放行：state=idle" state_field_is "$D33/state/zoe.json" state idle
assert "33.2 hook stop：owner 欄記錄 session_id" state_field_is "$D33/state/zoe.json" owner s33

# 33.3 hook stop：stop_hook_active=true＋不同 pending id → 照樣 block（連鎖合法）
D33c="$TESTROOT/d33c"
ab "$D33c" register zed "$PANE_A" 2>/dev/null
id33c="$(ab "$D33c" send zed --from alice --message a 2>/dev/null)"
TAGZED="ab-spawn-zed-777-aaaaaaaaaaaa"
mkdir -p "$D33c/state"
jq -n --arg s idle --arg t "2020-01-01T00:00:00Z" --arg ld "prev-fake-id" \
  '{state: $s, ts: $t, last_delivered: $ld}' > "$D33c/state/zed.json"
hookcall "$D33c" "$TAGZED" stop '{"stop_hook_active":true,"session_id":"s33c"}' > "$TESTROOT/h3.out" 2>/dev/null
assert "33.3 hook stop 連鎖：不同 pending id 仍 block" grep -q "receive $id33c" "$TESTROOT/h3.out"
assert "33.3 hook stop 連鎖：last_delivered 更新為新 id" \
  state_field_is "$D33c/state/zed.json" last_delivered "$id33c"

# 33.4 hook：無 AGENT_BRIDGE_SPAWN_TAG → exit 0、無輸出、不產生 state 目錄
D33d="$TESTROOT/d33d"
printf '{}' | env AGENT_BRIDGE_DATA="$D33d" PATH="$SHIM:$PATH" "$BRIDGE" hook stop \
  > "$TESTROOT/h4.out" 2>"$TESTROOT/h4.err"; rc=$?
assert "33.4 hook 無 tag：exit 0" test "$rc" -eq 0
assert "33.4 hook 無 tag：無 stdout" test ! -s "$TESTROOT/h4.out"
assert "33.4 hook 無 tag：不產生 state 目錄" bash -c "! test -e '$D33d/state'"

# 33.5 tag 析名：name 含連字號正確派對到 state 檔
D33e="$TESTROOT/d33e"
ab "$D33e" register my-worker-2 "$PANE_A" 2>/dev/null
id33e="$(ab "$D33e" send my-worker-2 --from alice --message e 2>/dev/null)"
TAGE="ab-spawn-my-worker-2-12345-0123456789ab"
hookcall "$D33e" "$TAGE" stop '{"session_id":"s33e"}' > "$TESTROOT/h5.out" 2>/dev/null
assert "33.5 tag 析名：連字號名稱派對到正確 state 檔" test -f "$D33e/state/my-worker-2.json"
assert "33.5 tag 析名：block JSON 含正確 id" grep -q "receive $id33e" "$TESTROOT/h5.out"

# 33.6 hook prompt-submit → busy；notification（idle 型）→ idle；其他型別 → state 不變
D33f="$TESTROOT/d33f"
ab "$D33f" register fin "$PANE_A" 2>/dev/null
TAGF="ab-spawn-fin-1-aaaaaaaaaaaa"
hookcall "$D33f" "$TAGF" prompt-submit '{"session_id":"s33f"}' >/dev/null 2>&1
assert "33.6 hook prompt-submit：state=busy" state_field_is "$D33f/state/fin.json" state busy
hookcall "$D33f" "$TAGF" notification '{"notification_type":"idle_prompt","session_id":"s33f"}' >/dev/null 2>&1
assert "33.6 hook notification(idle_prompt)：state=idle" state_field_is "$D33f/state/fin.json" state idle
hookcall "$D33f" "$TAGF" prompt-submit '{"session_id":"s33f"}' >/dev/null 2>&1
hookcall "$D33f" "$TAGF" notification '{"notification_type":"permission_prompt","session_id":"s33f"}' >/dev/null 2>&1
assert "33.6 hook notification(其他型別)：state 不變（仍 busy）" \
  state_field_is "$D33f/state/fin.json" state busy

# 33.7 send：state=busy 且新鮮 → send-keys 零次＋notify-deferred＋task 照建 queued
D33g="$TESTROOT/d33g"
p33g="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D33g" register gina "$p33g" 2>/dev/null
mkdir -p "$D33g/state"
jq -n --arg s busy --arg t "$(now_iso_test)" --arg ld "" \
  '{state: $s, ts: $t, last_delivered: $ld}' > "$D33g/state/gina.json"
tmx send-keys -t "$p33g" \
  "touch $TESTROOT/g-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/g-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/g-ready"
id33g="$(ab "$D33g" send gina --from alice --message hi 2>/dev/null)"
tmx send-keys -t "$p33g" 'SENTINEL-g' Enter
assert "33.7 state=busy 新鮮：記錄機制活著" wait_for 10 grep -q 'SENTINEL-g' "$TESTROOT/g-got.txt"
# shellcheck disable=SC2016  # $1/$2 由內層 bash 展開，刻意單引號
assert "33.7 state=busy 新鮮：got 只有哨兵一行（零次 send-keys 通知）" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -Fxq "$2" "$1"' _ "$TESTROOT/g-got.txt" 'SENTINEL-g'
assert "33.7 state=busy 新鮮：events.log 記 notify-deferred" \
  evt_grep "$D33g/tasks/$id33g/events.log" notify-deferred
assert "33.7 state=busy 新鮮：task 仍建立為 queued" st_is "$D33g" "$id33g" queued
tmx kill-pane -t "$p33g" 2>/dev/null || true

# 33.8 send：state=idle 且新鮮 → 通知照送
D33h="$TESTROOT/d33h"
p33h="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D33h" register hana "$p33h" 2>/dev/null
mkdir -p "$D33h/state"
jq -n --arg s idle --arg t "$(now_iso_test)" --arg ld "" \
  '{state: $s, ts: $t, last_delivered: $ld}' > "$D33h/state/hana.json"
tmx send-keys -t "$p33h" \
  "touch $TESTROOT/h-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/h-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/h-ready"
id33h="$(ab "$D33h" send hana --from alice --message hi 2>/dev/null)"
assert "33.8 state=idle 新鮮：pane 收到通知文字（照送）" wait_for 10 grep -q "$id33h" "$TESTROOT/h-got.txt"
assert "33.8 state=idle 新鮮：events.log 記 notified" evt_grep "$D33h/tasks/$id33h/events.log" notified
tmx kill-pane -t "$p33h" 2>/dev/null || true

# 33.9 send：state=busy 但 ts 過期 → 走 legacy 送鍵
D33i="$TESTROOT/d33i"
p33i="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D33i" register ivy "$p33i" 2>/dev/null
mkdir -p "$D33i/state"
jq -n --arg s busy --arg t "2000-01-01T00:00:00Z" --arg ld "" \
  '{state: $s, ts: $t, last_delivered: $ld}' > "$D33i/state/ivy.json"
tmx send-keys -t "$p33i" \
  "touch $TESTROOT/i-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/i-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/i-ready"
id33i="$(ab "$D33i" send ivy --from alice --message hi 2>/dev/null)"
assert "33.9 state=busy 但過期：走 legacy 送鍵（pane 收到通知文字）" \
  wait_for 10 grep -q "$id33i" "$TESTROOT/i-got.txt"
assert "33.9 state=busy 但過期：events.log 記 notified" evt_grep "$D33i/tasks/$id33i/events.log" notified
tmx kill-pane -t "$p33i" 2>/dev/null || true

# 33.10 send：state 檔損壞（非 JSON）→ legacy 路徑
D33j="$TESTROOT/d33j"
p33j="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D33j" register jojo "$p33j" 2>/dev/null
mkdir -p "$D33j/state"
printf 'not json{{{' > "$D33j/state/jojo.json"
tmx send-keys -t "$p33j" \
  "touch $TESTROOT/j-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/j-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/j-ready"
id33j="$(ab "$D33j" send jojo --from alice --message hi 2>/dev/null)"
assert "33.10 state 損壞：走 legacy 送鍵（pane 收到通知文字）" \
  wait_for 10 grep -q "$id33j" "$TESTROOT/j-got.txt"
assert "33.10 state 損壞：events.log 記 notified" evt_grep "$D33j/tasks/$id33j/events.log" notified
tmx kill-pane -t "$p33j" 2>/dev/null || true

# 33.11a respond_task（reply）通知 sender：busy→deferred
D33k="$TESTROOT/d33k"
p33k="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D33k" register kiwi "$p33k" 2>/dev/null
ab "$D33k" register lulu "$PANE_B" 2>/dev/null
id33k="$(ab "$D33k" send lulu --from kiwi --message hi 2>/dev/null)"
ab "$D33k" receive "$id33k" >/dev/null 2>&1
mkdir -p "$D33k/state"
jq -n --arg s busy --arg t "$(now_iso_test)" --arg ld "" \
  '{state: $s, ts: $t, last_delivered: $ld}' > "$D33k/state/kiwi.json"
tmx send-keys -t "$p33k" \
  "touch $TESTROOT/k-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/k-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/k-ready"
ab "$D33k" reply "$id33k" --message task-done 2>/dev/null
tmx send-keys -t "$p33k" 'SENTINEL-k' Enter
assert "33.11a reply 通知 sender busy→deferred：記錄機制活著" \
  wait_for 10 grep -q 'SENTINEL-k' "$TESTROOT/k-got.txt"
# shellcheck disable=SC2016  # $1/$2 由內層 bash 展開，刻意單引號
assert "33.11a reply 通知 sender busy→deferred：got 只有哨兵一行" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -Fxq "$2" "$1"' _ "$TESTROOT/k-got.txt" 'SENTINEL-k'
assert "33.11a reply 通知 sender busy→deferred：events.log 記 notify-deferred" \
  evt_grep "$D33k/tasks/$id33k/events.log" notify-deferred
tmx kill-pane -t "$p33k" 2>/dev/null || true

# 33.11b cancel 通知 to：busy→deferred
D33l="$TESTROOT/d33l"
p33l="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D33l" register momo2 "$p33l" 2>/dev/null
id33l="$(ab "$D33l" send momo2 --from alice --message hi 2>/dev/null)"
mkdir -p "$D33l/state"
jq -n --arg s busy --arg t "$(now_iso_test)" --arg ld "" \
  '{state: $s, ts: $t, last_delivered: $ld}' > "$D33l/state/momo2.json"
tmx send-keys -t "$p33l" \
  "touch $TESTROOT/l-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/l-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/l-ready"
ab "$D33l" cancel "$id33l" 2>/dev/null
tmx send-keys -t "$p33l" 'SENTINEL-l' Enter
assert "33.11b cancel 通知 to busy→deferred：記錄機制活著" \
  wait_for 10 grep -q 'SENTINEL-l' "$TESTROOT/l-got.txt"
# shellcheck disable=SC2016  # $1/$2 由內層 bash 展開，刻意單引號
assert "33.11b cancel 通知 to busy→deferred：got 只有哨兵一行" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -Fxq "$2" "$1"' _ "$TESTROOT/l-got.txt" 'SENTINEL-l'
assert "33.11b cancel 通知 to busy→deferred：events.log 記 notify-deferred" \
  evt_grep "$D33l/tasks/$id33l/events.log" notify-deferred
tmx kill-pane -t "$p33l" 2>/dev/null || true

# ---- 34. 獨立複核 blocker 修補（2026-07-28）----
# spec: HOOK-ID-3 HOOK-EVT-3 HOOK-NOTIFY-1 ENV-TTL-1 ENV-TTL-2

# 34.1a B1 反例：tasks/ 目錄名含 shell metacharacter，混在合法 queued task
# 之間 → hook stop 的 reason 只能含合法 id，惡意目錄名片段不得出現
D34a="$TESTROOT/d34a"
ab "$D34a" register quinn "$PANE_A" 2>/dev/null
id34a="$(ab "$D34a" send quinn --from alice --message t1 2>/dev/null)"
# shellcheck disable=SC2016  # 刻意單引號：$(id) 只是字面目錄名，不要展開
mal_name='EVIL$(id)-x; echo OWNED'
mkdir -p "$D34a/tasks/$mal_name"
jq -n --arg to quinn '{version: 1, to: $to, status: "queued"}' \
  > "$D34a/tasks/$mal_name/metadata.json"
printf 'queued\n' > "$D34a/tasks/$mal_name/status"
TAG34A="ab-spawn-quinn-1-aaaaaaaaaaaa"
hookcall "$D34a" "$TAG34A" stop '{"session_id":"s34a"}' > "$TESTROOT/h34a.out" 2>/dev/null
assert "34.1a B1：reason 含合法 id" grep -q "receive $id34a" "$TESTROOT/h34a.out"
# shellcheck disable=SC2016  # $1 由內層 bash 展開，刻意單引號
assert "34.1a B1：reason 不含惡意目錄名片段" \
  bash -c '! grep -qF "OWNED" "$1"' _ "$TESTROOT/h34a.out"
# shellcheck disable=SC2016  # $1 由內層 bash 展開，刻意單引號
assert "34.1a B1：reason 不含惡意目錄名片段（EVIL）" \
  bash -c '! grep -qF "EVIL" "$1"' _ "$TESTROOT/h34a.out"

# 34.1b B1 反例：只有惡意目錄、無合法 task → 不 block（無 stdout）、exit 0
D34b="$TESTROOT/d34b"
ab "$D34b" register quinn2 "$PANE_A" 2>/dev/null
# shellcheck disable=SC2016  # 刻意單引號：$(id) 只是字面目錄名，不要展開
mal_name2='EVIL$(id)-y; echo OWNED'
mkdir -p "$D34b/tasks/$mal_name2"
jq -n --arg to quinn2 '{version: 1, to: $to, status: "queued"}' \
  > "$D34b/tasks/$mal_name2/metadata.json"
printf 'queued\n' > "$D34b/tasks/$mal_name2/status"
TAG34B="ab-spawn-quinn2-1-aaaaaaaaaaaa"
hookcall "$D34b" "$TAG34B" stop '{"session_id":"s34b"}' > "$TESTROOT/h34b.out" 2>"$TESTROOT/h34b.err"; rc=$?
assert "34.1b B1：只有惡意目錄，exit 0" test "$rc" -eq 0
assert "34.1b B1：只有惡意目錄，無 stdout（不 block）" test ! -s "$TESTROOT/h34b.out"

# 34.2 B2 反例：state=busy 且 ts 在未來（2099）→ 不得永久新鮮，走 legacy 送鍵
D34c="$TESTROOT/d34c"
p34c="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D34c" register kay "$p34c" 2>/dev/null
mkdir -p "$D34c/state"
jq -n --arg s busy --arg t "2099-01-01T00:00:00Z" --arg ld "" \
  '{state: $s, ts: $t, last_delivered: $ld}' > "$D34c/state/kay.json"
tmx send-keys -t "$p34c" \
  "touch $TESTROOT/kay-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/kay-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/kay-ready"
id34c="$(ab "$D34c" send kay --from alice --message hi 2>/dev/null)"
assert "34.2 B2 反例：未來 ts 走 legacy 送鍵（pane 收到通知文字）" \
  wait_for 10 grep -q "$id34c" "$TESTROOT/kay-got.txt"
assert "34.2 B2 反例：events.log 記 notified" \
  evt_grep "$D34c/tasks/$id34c/events.log" notified
tmx kill-pane -t "$p34c" 2>/dev/null || true

# 34.3 B3 反例：hook stop 讀 stdin 不得無限期掛起
D34d="$TESTROOT/d34d"
TAG34D="ab-spawn-mo-1-aaaaaaaaaaaa"
timeout 10 env AGENT_BRIDGE_DATA="$D34d" AGENT_BRIDGE_SPAWN_TAG="$TAG34D" \
  PATH="$SHIM:$PATH" "$BRIDGE" hook stop <&- ; rc=$?
assert "34.3 B3 反例：fd 0 已關閉不掛起（rc=0，非 timeout 的 124）" test "$rc" -eq 0
timeout 10 env AGENT_BRIDGE_DATA="$D34d" AGENT_BRIDGE_SPAWN_TAG="$TAG34D" \
  PATH="$SHIM:$PATH" "$BRIDGE" hook stop < <(sleep 30) ; rc=$?
assert "34.3 B3 反例：管線開著不送資料不掛起（rc=0，非 timeout 的 124）" test "$rc" -eq 0

# 34.3b cross-vendor 複核 finding（2026-07-28）：上面兩條走的都是 timeout 在
# PATH 上的分支。缺 timeout 時的 fallback 曾是裸 cat，同樣的 stdin 永不關閉
# 就從另一條分支永久掛住 hook 呼叫端。這裡刻意造一個沒有 timeout 的最小 PATH。
# jq 與 mkdir 必須真的在裡面：cmd_hook 讀 stdin 之前就會因為缺這兩個而 exit 0，
# 少了它們這條測試會假綠（rc 為 0 但根本沒走到讀取那段）。
MINBIN="$TESTROOT/minbin"
mkdir -p "$MINBIN"
for t34 in bash cat jq mkdir dirname date; do
  p34="$(command -v "$t34" 2>/dev/null)" && ln -sf "$p34" "$MINBIN/$t34"
done
assert "34.3b 前提：最小 PATH 有 jq（缺了這條測試會假綠）" test -x "$MINBIN/jq"
assert "34.3b 前提：最小 PATH 有 mkdir（缺了這條測試會假綠）" test -x "$MINBIN/mkdir"
assert "34.3b 前提：最小 PATH 確實沒有 timeout" \
  bash -c "! PATH=\"$MINBIN\" command -v timeout >/dev/null 2>&1"
timeout 10 env AGENT_BRIDGE_DATA="$D34d" AGENT_BRIDGE_SPAWN_TAG="$TAG34D" \
  PATH="$MINBIN" "$BRIDGE" hook stop < <(sleep 30) ; rc=$?
assert "34.3b 反例：缺 timeout ＋ stdin 永不關閉仍不掛起（rc=0，非 124）" test "$rc" -eq 0

# 34.4 附帶：AGENT_BRIDGE_STATE_TTL=0 視為 state 通道關閉，一律走 legacy
D34e="$TESTROOT/d34e"
p34e="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D34e" register nia "$p34e" 2>/dev/null
mkdir -p "$D34e/state"
jq -n --arg s busy --arg t "$(now_iso_test)" --arg ld "" \
  '{state: $s, ts: $t, last_delivered: $ld}' > "$D34e/state/nia.json"
tmx send-keys -t "$p34e" \
  "touch $TESTROOT/nia-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/nia-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/nia-ready"
id34e="$(env AGENT_BRIDGE_STATE_TTL=0 AGENT_BRIDGE_DATA="$D34e" PATH="$SHIM:$PATH" \
  "$BRIDGE" send nia --from alice --message hi 2>/dev/null)"
assert "34.4 TTL=0：視為通道關閉，走 legacy 送鍵（pane 收到通知文字）" \
  wait_for 10 grep -q "$id34e" "$TESTROOT/nia-got.txt"
assert "34.4 TTL=0：events.log 記 notified" \
  evt_grep "$D34e/tasks/$id34e/events.log" notified
tmx kill-pane -t "$p34e" 2>/dev/null || true

# 34.4b ENV-TTL-2：通知端 STATE_TTL 壞值（非 0–9 位純數字）MUST die，
# 不得靜默採預設——hook 端壞值退預設是 ENV-TTL-3（34.5+），方向相反勿混。
# die 發生在 notify_or_defer 開頭、任何送鍵之前，PANE_A 不會收到雜訊
D34g="$TESTROOT/d34g"
ab "$D34g" register pia "$PANE_A" 2>/dev/null
env AGENT_BRIDGE_STATE_TTL=bad AGENT_BRIDGE_DATA="$D34g" PATH="$SHIM:$PATH" \
  "$BRIDGE" send pia --from alice --message hi >/dev/null 2>"$TESTROOT/h34g.err"; rc=$?
assert "34.4b ENV-TTL-2：send 於 TTL 壞值以錯誤終止" test "$rc" -ne 0
assert "34.4b ENV-TTL-2：錯誤訊息指向 STATE_TTL（排除其他死因）" \
  grep -q "AGENT_BRIDGE_STATE_TTL" "$TESTROOT/h34g.err"

# ---- 34.5+ 巢狀 runtime 冒名缺陷（owner/session_id 所有權閘門）----
# spec: HOOK-OWNER-1 HOOK-OWNER-2 HOOK-OWNER-3 HOOK-OWNER-4 HOOK-EVT-4 ENV-TTL-3 STATE-CHAN-2
# 缺陷紀錄：docs/codex-hooks-probe.md「巢狀 runtime 會汙染 parent 的 state」。
# SPAWN_TAG 被子行程繼承，僅靠 tag 分不出本尊與巢狀 session；閘門以 hook
# payload 的 session_id 先到先得認領 state 檔，異主且 state 新鮮一律靜默擋下
# （不寫 state、stop 不發 block），ts 過期或落在未來才允許接管（/clear 自癒）。

# 34.5 首次認領：空目錄第一個帶 session_id 的事件寫入 state 並記 owner
D34f="$TESTROOT/d34f"
ab "$D34f" register ora "$PANE_A" 2>/dev/null
TAG34F="ab-spawn-ora-1-aaaaaaaaaaaa"
hookcall "$D34f" "$TAG34F" prompt-submit '{"session_id":"sP"}' >/dev/null 2>&1
assert "34.5 首次認領：state=busy" state_field_is "$D34f/state/ora.json" state busy
assert "34.5 首次認領：owner=寫入者 session_id" state_field_is "$D34f/state/ora.json" owner sP

# 34.6 異主 notification：state 新鮮 → 靜默擋下，檔案逐位元不變
# 逐位元比對用 cmp 對事前副本，不用欄位比對——ts 只有秒級解析度，同秒內被
# 改寫再讀欄位會假綠
D34g="$TESTROOT/d34g"
ab "$D34g" register pia "$PANE_A" 2>/dev/null
TAG34G="ab-spawn-pia-1-aaaaaaaaaaaa"
mkdir -p "$D34g/state"
jq -n --arg s busy --arg t "$(now_iso_test)" --arg ld "" --arg o sP \
  '{state: $s, ts: $t, last_delivered: $ld, owner: $o}' > "$D34g/state/pia.json"
cp "$D34g/state/pia.json" "$TESTROOT/pia-before.json"
hookcall "$D34g" "$TAG34G" notification '{"notification_type":"idle_prompt","session_id":"sC"}' \
  > "$TESTROOT/h34g.out" 2>/dev/null; rc=$?
assert "34.6 異主 notification：exit 0" test "$rc" -eq 0
assert "34.6 異主 notification：無 stdout" test ! -s "$TESTROOT/h34g.out"
assert "34.6 異主 notification：state 檔逐位元不變" \
  cmp -s "$D34g/state/pia.json" "$TESTROOT/pia-before.json"

# 34.7 異主 stop：mailbox 有 queued task 也不得發 block（缺陷後果 2 的核心反證：
# 巢狀 session 不再被指使去 receive parent 的任務）
D34h="$TESTROOT/d34h"
ab "$D34h" register rex "$PANE_A" 2>/dev/null
id34h="$(ab "$D34h" send rex --from alice --message t1 2>/dev/null)"
TAG34H="ab-spawn-rex-1-aaaaaaaaaaaa"
mkdir -p "$D34h/state"
jq -n --arg s idle --arg t "$(now_iso_test)" --arg ld "" --arg o sP \
  '{state: $s, ts: $t, last_delivered: $ld, owner: $o}' > "$D34h/state/rex.json"
cp "$D34h/state/rex.json" "$TESTROOT/rex-before.json"
hookcall "$D34h" "$TAG34H" stop '{"session_id":"sC"}' > "$TESTROOT/h34h.out" 2>/dev/null; rc=$?
assert "34.7 異主 stop：exit 0" test "$rc" -eq 0
assert "34.7 異主 stop：無 stdout（不發 block）" test ! -s "$TESTROOT/h34h.out"
assert "34.7 異主 stop：task 仍 queued" st_is "$D34h" "$id34h" queued
assert "34.7 異主 stop：state 檔逐位元不變" \
  cmp -s "$D34h/state/rex.json" "$TESTROOT/rex-before.json"

# 34.8 本人 stop：同一目錄、owner 本人 → 照常 block 取件（gate 不誤擋正主）
hookcall "$D34h" "$TAG34H" stop '{"session_id":"sP"}' > "$TESTROOT/h34h2.out" 2>/dev/null
assert "34.8 本人 stop：block reason 含 receive <id>" grep -q "receive $id34h" "$TESTROOT/h34h2.out"
assert "34.8 本人 stop：last_delivered 更新" \
  state_field_is "$D34h/state/rex.json" last_delivered "$id34h"
assert "34.8 本人 stop：owner 維持本人" state_field_is "$D34h/state/rex.json" owner sP

# 34.9 過期接管：ts 超過 TTL 後新 session_id 可接管（/clear 換 id 的自癒路徑）
D34i="$TESTROOT/d34i"
ab "$D34i" register sol "$PANE_A" 2>/dev/null
TAG34I="ab-spawn-sol-1-aaaaaaaaaaaa"
mkdir -p "$D34i/state"
jq -n --arg s busy --arg t "2020-01-01T00:00:00Z" --arg ld "" --arg o sOld \
  '{state: $s, ts: $t, last_delivered: $ld, owner: $o}' > "$D34i/state/sol.json"
hookcall "$D34i" "$TAG34I" prompt-submit '{"session_id":"sNew"}' >/dev/null 2>&1
assert "34.9 過期接管：owner 換新 session_id" state_field_is "$D34i/state/sol.json" owner sNew
assert "34.9 過期接管：state=busy" state_field_is "$D34i/state/sol.json" state busy

# 34.10 未來 ts 接管：異主但 ts 在未來（2099）→ 不可信，允許接管（比照 34.2
# 的下界哲學：未來時間戳不得造成永久鎖死）
D34j="$TESTROOT/d34j"
ab "$D34j" register tam "$PANE_A" 2>/dev/null
TAG34J="ab-spawn-tam-1-aaaaaaaaaaaa"
mkdir -p "$D34j/state"
jq -n --arg s busy --arg t "2099-01-01T00:00:00Z" --arg ld "" --arg o sOld \
  '{state: $s, ts: $t, last_delivered: $ld, owner: $o}' > "$D34j/state/tam.json"
hookcall "$D34j" "$TAG34J" prompt-submit '{"session_id":"sNew"}' >/dev/null 2>&1
assert "34.10 未來 ts 接管：owner 換新 session_id" state_field_is "$D34j/state/tam.json" owner sNew

# 34.11 舊格式升級：無 owner 欄的三欄 state 檔視為無主，第一個帶 id 的事件認領
D34k="$TESTROOT/d34k"
ab "$D34k" register uma "$PANE_A" 2>/dev/null
TAG34K="ab-spawn-uma-1-aaaaaaaaaaaa"
mkdir -p "$D34k/state"
jq -n --arg s busy --arg t "$(now_iso_test)" --arg ld "" \
  '{state: $s, ts: $t, last_delivered: $ld}' > "$D34k/state/uma.json"
hookcall "$D34k" "$TAG34K" notification '{"notification_type":"idle_prompt","session_id":"sS"}' >/dev/null 2>&1
assert "34.11 舊格式無主：認領成功 state=idle" state_field_is "$D34k/state/uma.json" state idle
assert "34.11 舊格式無主：owner 記錄認領者" state_field_is "$D34k/state/uma.json" owner sS

# 34.12 異主不消耗防迴圈額度：stop 的「同 id 已擋過一輪」一次性放行只屬正主；
# 異主帶 stop_hook_active=true 也碰不到 last_delivered
D34l="$TESTROOT/d34l"
ab "$D34l" register vic "$PANE_A" 2>/dev/null
id34l="$(ab "$D34l" send vic --from alice --message t1 2>/dev/null)"
TAG34L="ab-spawn-vic-1-aaaaaaaaaaaa"
hookcall "$D34l" "$TAG34L" stop '{"session_id":"sP"}' > "$TESTROOT/h34l1.out" 2>/dev/null
assert "34.12 前置：正主首輪 block" grep -q "receive $id34l" "$TESTROOT/h34l1.out"
cp "$D34l/state/vic.json" "$TESTROOT/vic-mid.json"
hookcall "$D34l" "$TAG34L" stop '{"stop_hook_active":true,"session_id":"sC"}' \
  > "$TESTROOT/h34l2.out" 2>/dev/null
assert "34.12 異主 stop_hook_active：無 stdout" test ! -s "$TESTROOT/h34l2.out"
assert "34.12 異主 stop_hook_active：state 檔逐位元不變（額度未被消耗）" \
  cmp -s "$D34l/state/vic.json" "$TESTROOT/vic-mid.json"
hookcall "$D34l" "$TAG34L" stop '{"stop_hook_active":true,"session_id":"sP"}' \
  > "$TESTROOT/h34l3.out" 2>/dev/null
assert "34.12 正主同 id 放行：無 stdout" test ! -s "$TESTROOT/h34l3.out"
assert "34.12 正主同 id 放行：state=idle" state_field_is "$D34l/state/vic.json" state idle

# 34.14 gate 的 TTL 韌性（hook 鐵律）：TTL 壞值或 0 不得 die、不得變成
# 「無條件接管」——gate 內部退預設 1800 繼續運作，fresh 異主仍被靜默擋下
# （cross-vendor 複核 2026-07-29 suggestion 1，鎖住 R4）
D34n="$TESTROOT/d34n"
ab "$D34n" register xan "$PANE_A" 2>/dev/null
TAG34N="ab-spawn-xan-1-aaaaaaaaaaaa"
mkdir -p "$D34n/state"
jq -n --arg s busy --arg t "$(now_iso_test)" --arg ld "" --arg o sP \
  '{state: $s, ts: $t, last_delivered: $ld, owner: $o}' > "$D34n/state/xan.json"
cp "$D34n/state/xan.json" "$TESTROOT/xan-before.json"
printf '%s' '{"session_id":"sC"}' | env AGENT_BRIDGE_STATE_TTL=bad \
  AGENT_BRIDGE_DATA="$D34n" AGENT_BRIDGE_SPAWN_TAG="$TAG34N" PATH="$SHIM:$PATH" \
  "$BRIDGE" hook prompt-submit > "$TESTROOT/h34n1.out" 2>/dev/null; rc=$?
assert "34.14 TTL 壞值：exit 0（不 die）" test "$rc" -eq 0
assert "34.14 TTL 壞值：無 stdout" test ! -s "$TESTROOT/h34n1.out"
assert "34.14 TTL 壞值：fresh 異主仍被擋（state 逐位元不變）" \
  cmp -s "$D34n/state/xan.json" "$TESTROOT/xan-before.json"
printf '%s' '{"session_id":"sC"}' | env AGENT_BRIDGE_STATE_TTL=0 \
  AGENT_BRIDGE_DATA="$D34n" AGENT_BRIDGE_SPAWN_TAG="$TAG34N" PATH="$SHIM:$PATH" \
  "$BRIDGE" hook prompt-submit > "$TESTROOT/h34n2.out" 2>/dev/null; rc=$?
assert "34.14 TTL=0：exit 0（不 die）" test "$rc" -eq 0
assert "34.14 TTL=0：無 stdout" test ! -s "$TESTROOT/h34n2.out"
assert "34.14 TTL=0：不得變成無條件接管（state 逐位元不變）" \
  cmp -s "$D34n/state/xan.json" "$TESTROOT/xan-before.json"

# 34.13 缺 session_id：無身分者不參與 state 通道——不寫 state、不 block
# （失效方向＝既有降級鏈：state 停更 → TTL → legacy 送鍵）
D34m="$TESTROOT/d34m"
ab "$D34m" register wes "$PANE_A" 2>/dev/null
ab "$D34m" send wes --from alice --message t1 >/dev/null 2>&1
TAG34M="ab-spawn-wes-1-aaaaaaaaaaaa"
hookcall "$D34m" "$TAG34M" stop '{}' > "$TESTROOT/h34m.out" 2>/dev/null; rc=$?
assert "34.13 缺 sid：exit 0" test "$rc" -eq 0
assert "34.13 缺 sid：無 stdout（不 block）" test ! -s "$TESTROOT/h34m.out"
assert "34.13 缺 sid：不產生 state 檔" bash -c "! test -e '$D34m/state/wes.json'"

# ---- 35. 行程身分閘門（M5 窗 1）----
# spec: HOOK-OWNER-5 STATE-AGENT-4
# registry 帶得動 worker 的 (pid, starttime) 時，閘門比對 hook 行程的**直接**
# 父行程：確認得出是本尊就放行、不看 session_id／ts。這關掉的是「parent
# /clear 換新 session_id 後被自己的閘門擋住、要等 TTL 才自癒」那個窗——行程
# 沒變＝身分沒變。
#
# **只有相符是結論**。PPID 不符不代表冒名——合法中介（runtime fork 新主行程、
# 或使用者用 AGENT_BRIDGE_CLAUDE_HOOKS 指定 wrapper）一樣不符，當成冒名會把
# 本尊永久擋死，連 TTL 自癒都到不了。故不符一律落回 M4 的 session_id＋TTL。
# 設計依據與實測：docs/rust/m5-proposal.md。
#
# 這一組只對 Rust 正本執行：bash 正本凍結在 M4 的行為（rollback 基準），
# 不再長新功能。SKIP 是顯式的，不靜默跳過。
if [[ "$SRC_KIND" == rust ]]; then

# hookcall 的 pipeline 右段由當前 shell fork，故受測 hook 行程的直接父行程
# 就是這個 shell——$BASHPID 即「本尊」該有的 pid
ME_PID="$BASHPID"

# /proc/<pid>/stat 第 22 欄。comm（第 2 欄）可含空白與右括號，故從最後一個
# ") " 之後才開始數；切完 $1 是第 3 欄，第 22 欄因此是 ${20}
proc_starttime() {
  local raw
  raw="$(<"/proc/$1/stat")" || return 1
  raw="${raw##*') '}"
  # shellcheck disable=SC2086  # 刻意分詞：stat 欄位以空白分隔
  set -- $raw
  printf '%s\n' "${20}"
}

# 寫一份只含身分兩欄的 registry（其餘欄位與本組無關；閘門只讀這兩欄）
write_ident_reg() {
  local dir="$1" name="$2" pid="$3" start="$4"
  mkdir -p "$dir/agents"
  jq -n --arg p "$pid" --arg s "$start" \
    '{name: "x", pane_id: "%1", worker_pid: $p, worker_starttime: $s}' \
    > "$dir/agents/$name.json"
}

# 異主且新鮮的 state：沒有行程身分時這一定是「擋」，是本組所有對照的基準
seed_fresh_foreign_state() {
  local dir="$1" name="$2"
  mkdir -p "$dir/state"
  jq -n --arg t "$(now_iso_test)" \
    '{state: "busy", ts: $t, last_delivered: "", owner: "sP"}' \
    > "$dir/state/$name.json"
}

ME_START="$(proc_starttime "$ME_PID")"
assert "35 前置：取得本 shell 的 starttime" test -n "$ME_START"

# 35a 本尊：PPID 與 starttime 都對上 → 放行，且無視異主的 session_id
D35A="$TESTROOT/d35a"
seed_fresh_foreign_state "$D35A" ida
write_ident_reg "$D35A" ida "$ME_PID" "$ME_START"
hookcall "$D35A" "ab-spawn-ida-1-aaaaaaaaaaaa" prompt-submit '{"session_id":"sNEW"}' \
  >/dev/null 2>&1
assert "35a 本尊：異主 sid 仍放行（state 被寫入）" \
  state_field_is "$D35A/state/ida.json" state busy
assert "35a 本尊：owner 換成本次 session_id" \
  state_field_is "$D35A/state/ida.json" owner sNEW

# 35b PPID 不符（pid 指向一個活著、但不是本 hook 父行程的行程）→ 落回時間窗。
# state 新鮮且異主，落回後照樣擋——擋的是時間窗，不是身分。
# 用 tmux server 當那個「別人」：它一定活著且必然不是測試 shell 的子行程
OTHER_PID="$(tmux -L "$SOCK" display -p '#{pid}' 2>/dev/null || echo 1)"
OTHER_START="$(proc_starttime "$OTHER_PID")"
D35B="$TESTROOT/d35b"
seed_fresh_foreign_state "$D35B" idb
write_ident_reg "$D35B" idb "$OTHER_PID" "$OTHER_START"
cp "$D35B/state/idb.json" "$TESTROOT/idb-before.json"
hookcall "$D35B" "ab-spawn-idb-1-aaaaaaaaaaaa" prompt-submit '{"session_id":"sC"}' \
  > "$TESTROOT/h35b.out" 2>/dev/null; rc=$?
assert "35b PPID 不符：exit 0" test "$rc" -eq 0
assert "35b PPID 不符：無 stdout" test ! -s "$TESTROOT/h35b.out"
assert "35b PPID 不符＋state 新鮮：仍擋（state 檔逐位元不變）" \
  cmp -s "$D35B/state/idb.json" "$TESTROOT/idb-before.json"

# 35c **失效方向**：PPID 不符只是「確認不了」，不是「確認為冒名」。落回時間窗
# 後，ts 過期就該能接管。這是本組最該紅的一條——少了它，把不符當成確定冒名的
# 退化版本（本尊經合法中介呼叫時被永久擋死）會全綠通過
D35C="$TESTROOT/d35c"
mkdir -p "$D35C/state"
jq -n '{state: "busy", ts: "2020-01-01T00:00:00Z", last_delivered: "", owner: "sP"}' \
  > "$D35C/state/idc.json"
write_ident_reg "$D35C" idc "$OTHER_PID" "$OTHER_START"
hookcall "$D35C" "ab-spawn-idc-1-aaaaaaaaaaaa" prompt-submit '{"session_id":"sC"}' \
  >/dev/null 2>&1
assert "35c PPID 不符＋ts 過期：落回時間窗，可接管" \
  state_field_is "$D35C/state/idc.json" owner sC

# 35d 記錄過期（pid 還在但 starttime 對不上＝pid 被重用）→ 落回時間窗。
# 落回之後 ts 過期，於是可接管——證明「無法裁決」與「裁決為冒名」不同路
D35D="$TESTROOT/d35d"
mkdir -p "$D35D/state"
jq -n '{state: "busy", ts: "2020-01-01T00:00:00Z", last_delivered: "", owner: "sP"}' \
  > "$D35D/state/idd.json"
write_ident_reg "$D35D" idd "$ME_PID" 999999999999
hookcall "$D35D" "ab-spawn-idd-1-aaaaaaaaaaaa" prompt-submit '{"session_id":"sTAKE"}' \
  >/dev/null 2>&1
assert "35d 記錄過期：落回時間窗，過期 state 可接管" \
  state_field_is "$D35D/state/idd.json" owner sTAKE

# 35e 欄位不全 → 落回。三種形狀各驗一次（缺一欄、兩欄皆空、無 registry）
i=0
for reg_doc in '{"worker_pid":"'"$ME_PID"'"}' '{"worker_pid":"","worker_starttime":""}' ''; do
  i=$((i + 1))
  DE="$TESTROOT/d35e$i"
  mkdir -p "$DE/state"
  jq -n '{state: "busy", ts: "2020-01-01T00:00:00Z", last_delivered: "", owner: "sP"}' \
    > "$DE/state/ide.json"
  if [[ -n "$reg_doc" ]]; then
    mkdir -p "$DE/agents"
    printf '%s\n' "$reg_doc" > "$DE/agents/ide.json"
  fi
  hookcall "$DE" "ab-spawn-ide-1-aaaaaaaaaaaa" prompt-submit '{"session_id":"sFB"}' \
    >/dev/null 2>&1
  assert "35e-$i 身分欄不全：落回時間窗（過期可接管）" \
    state_field_is "$DE/state/ide.json" owner sFB
done

# 35g 合法中介：registry 記的 pid 是本尊（本 shell）且它還活著，但 hook 經一層
# 合法 wrapper 呼叫，於是直接 PPID 是那層 wrapper。這是本輪修復的行為錨——
# 把不符當成確定的冒名，本尊會被永久擋死，連 TTL 都救不回來。
#
# `; exit 0` 是必要的：bash -c 對「唯一且最後一個」命令會直接 exec 取代自己，
# 那樣就沒有中介行程了。兩條斷言合起來才成立——第一條證明中介真的存在（PPID
# 確實不符，否則會被當本尊放行），第二條證明不符只是落回而非擋死
hookcall_via_wrapper() {
  local data="$1" tag="$2" json="$3"
  printf '%s' "$json" | bash --norc --noprofile -c \
    'env AGENT_BRIDGE_DATA="$1" AGENT_BRIDGE_SPAWN_TAG="$2" PATH="$3" "$4" hook prompt-submit
     exit 0' _ "$data" "$tag" "$SHIM:$PATH" "$BRIDGE"
}

D35G="$TESTROOT/d35g"
seed_fresh_foreign_state "$D35G" idg
write_ident_reg "$D35G" idg "$ME_PID" "$ME_START"
cp "$D35G/state/idg.json" "$TESTROOT/idg-before.json"
hookcall_via_wrapper "$D35G" "ab-spawn-idg-1-aaaaaaaaaaaa" '{"session_id":"sMID"}' \
  >/dev/null 2>&1
assert "35g 合法中介：PPID 確實不符（state 新鮮異主，故擋）" \
  cmp -s "$D35G/state/idg.json" "$TESTROOT/idg-before.json"

D35G2="$TESTROOT/d35g2"
mkdir -p "$D35G2/state"
jq -n '{state: "busy", ts: "2020-01-01T00:00:00Z", last_delivered: "", owner: "sP"}' \
  > "$D35G2/state/idg.json"
write_ident_reg "$D35G2" idg "$ME_PID" "$ME_START"
hookcall_via_wrapper "$D35G2" "ab-spawn-idg-1-aaaaaaaaaaaa" '{"session_id":"sMID"}' \
  >/dev/null 2>&1
assert "35g 合法中介＋ts 過期：不得永久擋死本尊（可接管）" \
  state_field_is "$D35G2/state/idg.json" owner sMID

# 35f STATE-AGENT-4：spawn 真的把身分兩欄寫進 registry，且 worker_pid 指向的
# 行程確實是本次 runtime。少了這條，上面幾條驗的都只是自己捏的 registry，
# 「spawn 會不會寫」完全沒被鎖住
D35F="$TESTROOT/d35f"
pane_35f="$(absp "$D35F" 20 spawn wid --runtime codex 2>/dev/null)"
assert "35f spawn 寫入 worker_pid（純數字）" \
  jq -e '.worker_pid | test("^[0-9]+$")' "$D35F/agents/wid.json"
assert "35f spawn 寫入 worker_starttime（純數字）" \
  jq -e '.worker_starttime | test("^[0-9]+$")' "$D35F/agents/wid.json"
# 記到的必須是這個 pane 的行程，而且是同一次行程生命。
# **刻意不在這裡事後比對 argv**：runtime stub 收工前會 `exec bash` 換掉自己的
# argv（pid 不變、starttime 不變）——這正好示範了為什麼持續判別要綁 starttime
# 而不是綁 argv。argv 只在 spawn 當下用來確認「pane_pid 是 runtime 不是中介
# shell」，那一層由 spawn::tests::worker_identity_requires_runtime_attestation
# 守——它錨在 caller 上，把 resolve_worker_identity 裡的 attestation 拿掉會紅
# （只驗 helper 的單元測試擋不住那個退化，codex 複核 2026-07-31 §4）
wid_pid="$(jq -r '.worker_pid' "$D35F/agents/wid.json")"
assert "35f worker_pid 等於 tmux 回報的 pane_pid" \
  test "$wid_pid" = "$(tmx display -pt "$pane_35f" '#{pane_pid}')"
assert "35f worker_starttime 與該 pid 當下的 starttime 相符" \
  bash -c "[ \"\$(bash -c 'raw=\$(</proc/$wid_pid/stat); raw=\${raw##*\") \"}; set -- \$raw; echo \${20}')\" = \"$(jq -r '.worker_starttime' "$D35F/agents/wid.json")\" ]"
absp "$D35F" 5 despawn wid >/dev/null 2>&1

else
  printf 'SKIP: 35 行程身分閘門（SRC_KIND=bash：bash 正本凍結在 M4 行為，不含 M5）\n'
fi

# ---- 36. codex launcher 形（HOOK-OWNER-5 自癒擴充）----
# spec: HOOK-OWNER-5
# codex 是 node launcher：pane_pid fork 原生執行檔、原生執行檔才 fork hook，
# 直接形永遠對不上。擴充：registry `runtime=codex` 時另試 launcher 形——hook
# 的直接 PPID 命中 codex argv 形、且其父行程即 worker_pid，恰好一層。
#
# **本組的主要斷言是「巢狀不被誤放行」，不是「codex 本尊會綠」**：誤放行＝
# 立即奪權（不經 TTL），是身分閘門唯一比 M4 更糟的失效方向。實測巢狀鏈
# （m5-proposal §1）是 hook → 巢狀 claude → bash → 本尊——巢狀正是 hook 的
# 直接 PPID，任何「祖先鏈走得到本尊」「鏈上沒夾別的 runtime」式的放寬都會
# 把它確認成本尊（architecture §11.7）。中介一律用**真的行程**構造（argv 形
# 狀由 script 名決定），不是 registry 捏的假資料。
#
# 與 35 同理只對 Rust 執行；bash 正本凍結、顯式 SKIP。
if [[ "$SRC_KIND" == rust ]]; then

# 假 runtime 中介：bash script，argv 為 ["/bin/bash", "…/rtbin36/<名>"]，依
# STATE-AGENT-4 前兩項規則命中 <名>。hook 呼叫後接 exit 0——非最後命令，防
# bash 把唯一命令 exec 合併掉（那樣中介行程就不存在了，同 35g）
RTBIN36="$TESTROOT/rtbin36"
mkdir -p "$RTBIN36"
for rt36 in codex claude; do
  # shellcheck disable=SC2016  # 寫的是字面 script 內容，展開發生在中介行程裡
  printf '%s\n' '#!/bin/bash' '"$AB_BRIDGE" hook prompt-submit' 'exit 0' \
    > "$RTBIN36/$rt36"
  chmod +x "$RTBIN36/$rt36"
done

# 經一層假 runtime 中介呼叫 hook。pipeline 右段由本 shell fork 後 exec，故
# 中介的父行程是本 shell（$ME_PID）——正是 launcher 形的「worker fork 中介、
# 中介 fork hook」形狀
hookcall_via_rt36() {
  local rt="$1" data="$2" tag="$3" json="$4"
  printf '%s' "$json" | env AGENT_BRIDGE_DATA="$data" AGENT_BRIDGE_SPAWN_TAG="$tag" \
    PATH="$SHIM:$PATH" AB_BRIDGE="$BRIDGE" "$RTBIN36/$rt"
}

# registry 含 runtime 欄的身分三欄版
write_ident_reg_rt36() {
  local dir="$1" name="$2" pid="$3" start="$4" rt="$5"
  mkdir -p "$dir/agents"
  jq -n --arg p "$pid" --arg s "$start" --arg r "$rt" \
    '{name: "x", pane_id: "%1", worker_pid: $p, worker_starttime: $s, runtime: $r}' \
    > "$dir/agents/$name.json"
}

# 36a launcher 形本尊：runtime=codex、中介命中 codex 形、中介的父行程＝記錄
# 的 worker_pid → 放行，無視異主的新鮮 state（即時自癒）
D36A="$TESTROOT/d36a"
seed_fresh_foreign_state "$D36A" ija
write_ident_reg_rt36 "$D36A" ija "$ME_PID" "$ME_START" codex
hookcall_via_rt36 codex "$D36A" "ab-spawn-ija-1-aaaaaaaaaaaa" '{"session_id":"sNEW"}' \
  >/dev/null 2>&1
assert "36a codex launcher 形本尊：異主 sid 仍放行（state 被寫入）" \
  state_field_is "$D36A/state/ija.json" state busy
assert "36a codex launcher 形本尊：owner 換成本次 session_id" \
  state_field_is "$D36A/state/ija.json" owner sNEW

# 36b 直接形對 codex 照樣成立：launcher 形是「另試」，不是取代
D36B="$TESTROOT/d36b"
seed_fresh_foreign_state "$D36B" ijb
write_ident_reg_rt36 "$D36B" ijb "$ME_PID" "$ME_START" codex
hookcall "$D36B" "ab-spawn-ijb-1-aaaaaaaaaaaa" prompt-submit '{"session_id":"sNEW"}' \
  >/dev/null 2>&1
assert "36b codex 直接形本尊：照樣放行" \
  state_field_is "$D36B/state/ijb.json" owner sNEW

# 36c **巢狀反例（本組最重要）**：claude worker 下掛一層真的 claude 形狀行程
# （hook → 巢狀 claude → 本尊），launcher 形 MUST NOT 對 claude 啟用。放寬成
# 「攀鏈到本尊即確認」或「按記錄的 runtime 名認中介」的退化版本，這條會紅
D36C="$TESTROOT/d36c"
seed_fresh_foreign_state "$D36C" ijc
write_ident_reg_rt36 "$D36C" ijc "$ME_PID" "$ME_START" claude
cp "$D36C/state/ijc.json" "$TESTROOT/ijc-before.json"
hookcall_via_rt36 claude "$D36C" "ab-spawn-ijc-1-aaaaaaaaaaaa" '{"session_id":"sNEST"}' \
  > "$TESTROOT/h36c.out" 2>/dev/null; rc=$?
assert "36c claude 巢狀：exit 0" test "$rc" -eq 0
assert "36c claude 巢狀：無 stdout" test ! -s "$TESTROOT/h36c.out"
assert "36c claude 巢狀：不得確認（state 逐位元不變）" \
  cmp -s "$D36C/state/ijc.json" "$TESTROOT/ijc-before.json"

# 36c2 同構但 ts 過期：落回的是時間窗，不是「確認為冒名」——過期仍可接管
D36C2="$TESTROOT/d36c2"
mkdir -p "$D36C2/state"
jq -n '{state: "busy", ts: "2020-01-01T00:00:00Z", last_delivered: "", owner: "sP"}' \
  > "$D36C2/state/ijc.json"
write_ident_reg_rt36 "$D36C2" ijc "$ME_PID" "$ME_START" claude
hookcall_via_rt36 claude "$D36C2" "ab-spawn-ijc-1-aaaaaaaaaaaa" '{"session_id":"sMID"}' \
  >/dev/null 2>&1
assert "36c2 claude 巢狀＋ts 過期：落回時間窗，可接管" \
  state_field_is "$D36C2/state/ijc.json" owner sMID

# 36d 鏈更長（巢狀 codex 的形狀）：中介命中 codex 形，但其父行程是多出來的
# bash 夾層而非 worker_pid——「恰好一層」的上界。巢狀 codex 的真實鏈至少長
# 這樣（hook → 原生 → launcher → shell → …），一律落回
D36D="$TESTROOT/d36d"
seed_fresh_foreign_state "$D36D" ijd
write_ident_reg_rt36 "$D36D" ijd "$ME_PID" "$ME_START" codex
cp "$D36D/state/ijd.json" "$TESTROOT/ijd-before.json"
# shellcheck disable=SC2016  # '"$1"' 由夾層 bash 展開，正是要多出來的那層
printf '%s' '{"session_id":"sDEEP"}' | env AGENT_BRIDGE_DATA="$D36D" \
  AGENT_BRIDGE_SPAWN_TAG="ab-spawn-ijd-1-aaaaaaaaaaaa" PATH="$SHIM:$PATH" \
  AB_BRIDGE="$BRIDGE" bash --norc --noprofile -c '"$1"; exit 0' _ "$RTBIN36/codex" \
  >/dev/null 2>&1
assert "36d 鏈更長（codex 中介隔著 bash 夾層）：不得確認" \
  cmp -s "$D36D/state/ijd.json" "$TESTROOT/ijd-before.json"

# 36e 中介不命中本 runtime 形：codex worker 下的 claude 形中介（巢狀 claude
# 掛在 codex worker 裡）→ 落回
D36E="$TESTROOT/d36e"
seed_fresh_foreign_state "$D36E" ije
write_ident_reg_rt36 "$D36E" ije "$ME_PID" "$ME_START" codex
cp "$D36E/state/ije.json" "$TESTROOT/ije-before.json"
hookcall_via_rt36 claude "$D36E" "ab-spawn-ije-1-aaaaaaaaaaaa" '{"session_id":"sX"}' \
  >/dev/null 2>&1
assert "36e codex worker 下的 claude 形中介：不得確認" \
  cmp -s "$D36E/state/ije.json" "$TESTROOT/ije-before.json"

# 36g 白名單交叉反例：claude worker 下掛一個**codex 形**的中介、其父又正好
# 是 worker_pid——launcher 形的結構全對，但 runtime=claude 不在白名單 → MUST
# 落回。這條架的是「runtime 白名單開關」本身：只刪 runtime guard 而保留
# 硬編碼 codex attest 的退化，36c 照不出來（claude 形中介 attest 不中），
# 唯獨這條會紅（codex 跨廠複核 2026-07-31 major 1）
D36G="$TESTROOT/d36g"
seed_fresh_foreign_state "$D36G" ijg
write_ident_reg_rt36 "$D36G" ijg "$ME_PID" "$ME_START" claude
cp "$D36G/state/ijg.json" "$TESTROOT/ijg-before.json"
hookcall_via_rt36 codex "$D36G" "ab-spawn-ijg-1-aaaaaaaaaaaa" '{"session_id":"sXRT"}' \
  >/dev/null 2>&1
assert "36g claude worker 下的 codex 形中介（結構全對）：白名單不含 claude，不得確認" \
  cmp -s "$D36G/state/ijg.json" "$TESTROOT/ijg-before.json"

# 36f 鏈中斷：中介活著、但記錄的 worker（中介之父）已死——hook 執行時中介已
# 被 reparent，攀不回 worker_pid。用真的行程死亡構造，不靠推論：subshell 寫完
# registry（記自己為 worker）、背景起 codex 形中介後立即退出；中介等 subshell
# 死透才跑 hook，完成後落 done 標記
D36F="$TESTROOT/d36f"
seed_fresh_foreign_state "$D36F" ijf
cp "$D36F/state/ijf.json" "$TESTROOT/ijf-before.json"
printf '%s' '{"session_id":"sORPH"}' > "$TESTROOT/ijf.json"
# 中介必須命中 codex 形（argv[0]=bash、argv[1] basename=codex），讓「上游已
# 死」成為唯一的落回原因——與 36e 的 attest 不命中路徑分開
RTBIN36O="$TESTROOT/rtbin36o"
mkdir -p "$RTBIN36O"
# 等 reparent 不能用 `kill -0 舊父`：父行程死後可能殘留 zombie（視 reaper
# 而定），kill -0 對 zombie 照樣成功。改輪詢**自己的 ppid 是否已不是舊父**
# ——reparent 正是「上游已死」的直接可觀察形狀。有界輪詢，逾時放棄（無 done
# 標記，前置斷言會紅）
# shellcheck disable=SC2016  # 寫的是字面 script 內容，展開發生在中介行程裡
printf '%s\n' '#!/bin/bash' \
  'n=0' \
  'while raw=$(</proc/self/stat); raw=${raw##*") "}; set -- $raw; [[ "$2" == "$AB_DEAD_PPID" ]]; do' \
  '  sleep 0.02; n=$((n+1)); [[ $n -gt 500 ]] && exit 1' \
  'done' \
  '"$AB_BRIDGE" hook prompt-submit < "$AB_JSON_FILE"' \
  ': > "$AB_DONE_FILE"' 'exit 0' > "$RTBIN36O/codex"
chmod +x "$RTBIN36O/codex"
(
  write_ident_reg_rt36 "$D36F" ijf "$BASHPID" "$(proc_starttime "$BASHPID")" codex
  env AGENT_BRIDGE_DATA="$D36F" AGENT_BRIDGE_SPAWN_TAG="ab-spawn-ijf-1-aaaaaaaaaaaa" \
    PATH="$SHIM:$PATH" AB_BRIDGE="$BRIDGE" AB_DEAD_PPID="$BASHPID" \
    AB_JSON_FILE="$TESTROOT/ijf.json" AB_DONE_FILE="$TESTROOT/ijf.done" \
    bash "$RTBIN36O/codex" >/dev/null 2>&1 &
)
# 等中介跑完 hook（有界輪詢，不靠猜時序）
for _ in $(seq 1 200); do [[ -e "$TESTROOT/ijf.done" ]] && break; sleep 0.05; done
assert "36f 鏈中斷前置：中介確實完成了 hook 呼叫" test -e "$TESTROOT/ijf.done"
assert "36f 鏈中斷（worker 已死、中介被 reparent）：不得確認" \
  cmp -s "$D36F/state/ijf.json" "$TESTROOT/ijf-before.json"

else
  printf 'SKIP: 36 codex launcher 形（SRC_KIND=bash：bash 正本凍結在 M4 行為，不含 M5）\n'
fi

# ---- 37. agy runtime（Antigravity CLI）----
# spec: CLI-SPAWN-1 HOOK-NOTIFY-2
# 第三個 runtime。量測正本 docs/agy-probe.md：`--prompt-interactive` 吃空白
# 分隔的旗標值，位置參數原樣落為初始 prompt，故沿用同一條 worker_prompt_arg
# 注入路徑；旗標姿態（skip-permissions＋sandbox）是使用者裁決，不是實作偏好，
# 因此用白名單式 argv 斷言鎖住——與 16a2 同理，子字串比對擋不住偷偷追加的旗標。
#
# 與 35／36 同理只對 Rust 執行；bash 正本自 M4 凍結、不支援 agy，顯式 SKIP。
if [[ "$SRC_KIND" == rust ]]; then

# 37a 快樂路徑＋argv 白名單
pane_wy="$(absp "$DSPAWN" 20 spawn wy1 --runtime agy 2>/dev/null)"; rc=$?
assert "37a spawn --runtime agy：exit 0" test "$rc" -eq 0
assert "37a spawn stdout 只印 pane-id（%N）一行" \
  bash -c "[[ '$pane_wy' =~ ^%[0-9]+\$ ]]"
assert "37a agy registry：runtime 欄為 agy" \
  jq -e '.runtime == "agy"' "$DSPAWN/agents/wy1.json"
assert "37a agy 一樣注入 worker brief（走同一條 worker_prompt_arg）" \
  grep -q -- 'The above is your worker brief' "$TESTROOT/agy-args.txt"
# 白名單：argv 必須「恰好」是 --dangerously-skip-permissions / --sandbox /
# --prompt-interactive / <prompt> 四個。少一個旗標＝姿態被悄悄放寬（例如
# 掉了 --sandbox），多一個＝有人往 worker 啟動旗標塞東西，兩個方向都該紅
assert "37a agy argv 恰好四個參數（追加或遺漏任何旗標都該紅）" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/agy-argv.txt'; (( \${#A[@]} == 4 ))"
assert "37a agy argv 前三個恰為裁決的旗標、第四個是 prompt 非旗標" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/agy-argv.txt'; [[ \${A[0]} == '--dangerously-skip-permissions' && \${A[1]} == '--sandbox' && \${A[2]} == '--prompt-interactive' && \${A[3]} != -* ]]"
# 否定式斷言先證明「有東西可否定」（16a2 的空綠教訓）：-p 會讓 pane 跑完即退
assert "37a agy 不帶 -p/--print（headless 會讓 pane 跑完即退）" \
  bash -c "[[ -s '$TESTROOT/agy-args.txt' ]] && ! grep -qE -- '(^| )(-p|--print)( |\$)' '$TESTROOT/agy-args.txt'"
assert "37a agy 探針重送生效：ready == true" \
  jq -e '.ready == true' "$DSPAWN/agents/wy1.json"
assert "37a agents.log 記 spawned wy1 … agy" \
  grep -qE "Z spawned wy1 ${pane_wy} agy -\$" "$DSPAWN/agents.log"

# 37b --model 下放。**順序是語意，不是排版**：`--prompt-interactive` 不是布林
# 開關而是吃下一個 token 當值的 string flag（實測 `agy --prompt-interactive
# --not-a-real-flag --help` rc=0，未知旗標被吞成值而非報錯）。它必須是最後一個
# 旗標，否則 `--model` 會被吃成 initial prompt、模型旗標失效、真 prompt 錯位。
# shim 只記 argv 不解析 agy 旗標，所以錯序照樣「綠」——這條斷言鎖的就是那個
# shim 看不見的語意（跨廠複核 2026-07-31 抓出實作錯序，本斷言曾一度鎖住錯形）
absp "$DSPAWN" 20 spawn wy2 --runtime agy --model gemini-3.6-flash-high >/dev/null 2>&1; rc=$?
assert "37b spawn --runtime agy --model：exit 0" test "$rc" -eq 0
assert "37b agy argv 恰好六個參數（37a 的四個 ＋ --model <m>）" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/agy-argv.txt'; (( \${#A[@]} == 6 ))"
assert "37b agy argv 完整序列：--model 在 --prompt-interactive **之前**、prompt 緊接其後且在最後" \
  bash -c "mapfile -d '' -t A < '$TESTROOT/agy-argv.txt'; [[ \${A[0]} == '--dangerously-skip-permissions' && \${A[1]} == '--sandbox' && \${A[2]} == '--model' && \${A[3]} == 'gemini-3.6-flash-high' && \${A[4]} == '--prompt-interactive' && \${A[5]} != -* ]]"
assert "37b agy worker 可正常 despawn" ab "$DSPAWN" despawn wy2
assert "37a agy worker 可正常 despawn" ab "$DSPAWN" despawn wy1

# 37c agy 無 hooks → 不寫 state → 通知端一律走 legacy 送鍵（CLI-SPAWN-1 的
# Note）。鎖的是「缺 state 檔時通知照樣送達 agy worker」——降級鏈本身在分組
# 33/34 已鎖，這裡確認 agy 這個 runtime 落在該鏈的「未知」分支而非別處
D37="$TESTROOT/d37"
p_agy="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D37" register wy3 "$p_agy" 2>/dev/null
tmx send-keys -t "$p_agy" \
  "touch $TESTROOT/agy37c-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/agy37c-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/agy37c-ready"
id37c="$(ab "$D37" send wy3 --from alice --message hi 2>/dev/null)"
assert "37c 本組佈景確認：該 agent 無 state 檔（真機無 hooks 的事實見 probe）" \
  test ! -e "$D37/state/wy3.json"
assert "37c 無 state → legacy 送鍵照樣送達（got 收到含 task-id 的 receive 行）" \
  wait_for 10 grep -q "$id37c" "$TESTROOT/agy37c-got.txt"
tmx kill-pane -t "$p_agy" 2>/dev/null || true

# 37d AGY-PROMPT-1：agy 權限框的送鍵防線。agy 的框 footer 是**小寫**
# `esc to cancel`，claude 的兩組特徵一個都不命中；不補的話送鍵的 Enter 會落在
# 預設選項 `1. Yes`，替一個正等人類決策的 worker 按下批准。錨是 agy 獨有的
# `Requesting permission for:`（不放寬 esc 大小寫的理由見 notify.rs 註解）。
# 畫面文字逐字抄自實測（docs/agy-probe.md）。攔截判準沿用 8a 的哨兵法：
# 先證明記錄機制活著，再斷言 got 恰好只有哨兵一行——只驗「不含 task-id」擋不住
# 「只送了一個 Enter」這種 regression，而那個 Enter 正是會誤批的東西
#
# pane 開在**自己的視窗**而非 split 進 `it`：跑全套時 `it` 已累積多個 pane，
# 每個只剩幾列高，六行框加上折行的指令列會超過可見高度，特徵被捲出「一屏」
# 之外——`screen_has_prompt` 依契約只看一屏可見文字，那是測試佈景的假紅，
# 不是防線失效（單跑分組 37 全綠、全套紅，實測 2026-07-31）。
D37P="$TESTROOT/d37p"
p_agyask="$(tmx new-window -dP -F '#{pane_id}' "$pane_cmd")"
ab "$D37P" register wy4 "$p_agyask" 2>/dev/null
tmx send-keys -t "$p_agyask" \
  "printf '%s\n' 'Requesting permission for:' '   ./bin/agent-bridge receive t1' 'Do you want to proceed?' '> 1. Yes' '  4. No' 'esc to cancel' ; touch $TESTROOT/agyask-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/agyask-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/agyask-ready"
id37d="$(ab "$D37P" send wy4 --from alice --message hi 2>/dev/null)"; rc=$?
tmx send-keys -t "$p_agyask" 'SENTINEL-agy' Enter
assert "37d agy 框偵測：send 仍 exit 0" test "$rc" -eq 0
assert "37d agy 框偵測：記錄機制活著（哨兵行有進 got）" \
  wait_for 10 grep -q 'SENTINEL-agy' "$TESTROOT/agyask-got.txt"
# shellcheck disable=SC2016  # $1/$2 由內層 bash 展開，刻意單引號
assert "37d agy 權限框在場時一個按鍵都沒送進去（否則 Enter 替 worker 按下 1. Yes）" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -Fxq "$2" "$1"' _ "$TESTROOT/agyask-got.txt" 'SENTINEL-agy'
assert "37d agy 框偵測：events.log 記 notify-failed" \
  evt_grep "$D37P/tasks/$id37d/events.log" notify-failed
tmx kill-window -t "$p_agyask" 2>/dev/null || true

# 37e **矮 pane（production 形狀）**：worker 預設進共用 window 並 tiled 均分，
# pane 一多就矮到框的 header 捲出可見一屏——掃描只看一屏（capture-pane -pJ，
# 不取 scrollback），那時 `Requesting permission for:` 已不在畫面。37d 用獨立
# 視窗驗完整框，這條驗只剩下緣的情形：header 不可見、選項與小寫 footer 仍在。
# 單靠 header 錨的版本在這裡必紅（跨廠複核 2026-07-31 的 blocker）
D37S="$TESTROOT/d37s"
w_short="$(tmx new-window -dP -F '#{window_id}' "$pane_cmd")"
p_short="$(tmx list-panes -t "$w_short" -F '#{pane_id}')"
# 把視窗壓到 6 列：框只放得下下緣（問句＋兩個選項＋footer），header 進 scrollback
tmx resize-window -t "$w_short" -x 200 -y 6 2>/dev/null || true
tmx send-keys -t "$p_short" \
  "printf '%s\n' 'Requesting permission for:' '   ./bin/agent-bridge receive t1' 'Do you want to proceed?' '> 1. Yes' '  4. No' 'esc to cancel' ; touch $TESTROOT/agyshort-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/agyshort-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/agyshort-ready"
# 前置斷言：確認這一屏真的看不到 header——否則本組退化成 37d 的複本、什麼都沒鎖。
# 先把可見一屏擷取到檔案再斷言：`tmx` 是本殼的 function，`bash -c` 子殼裡不存在，
# 直接寫進 assert 的子殼會讓否定式前置**假綠**（找不到指令 → 無輸出 → ! 成立）
tmx capture-pane -pJ -t "$p_short" > "$TESTROOT/agyshort-screen.txt"
assert "37e 前置：矮 pane 的可見一屏已看不到 header 錨" \
  bash -c "! grep -qF 'Requesting permission for:' '$TESTROOT/agyshort-screen.txt'"
assert "37e 前置：下緣特徵仍在可見一屏" \
  grep -qF 'esc to cancel' "$TESTROOT/agyshort-screen.txt"
ab "$D37S" register wy5 "$p_short" 2>/dev/null
id37e="$(ab "$D37S" send wy5 --from alice --message hi 2>/dev/null)"
tmx send-keys -t "$p_short" 'SENTINEL-short' Enter
assert "37e 矮 pane 記錄機制活著（哨兵行有進 got）" \
  wait_for 10 grep -q 'SENTINEL-short' "$TESTROOT/agyshort-got.txt"
# shellcheck disable=SC2016  # $1/$2 由內層 bash 展開，刻意單引號
assert "37e header 捲出一屏時仍 MUST NOT 送鍵（下緣備援錨接住）" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -Fxq "$2" "$1"' _ "$TESTROOT/agyshort-got.txt" 'SENTINEL-short'
assert "37e events.log 記 notify-failed" \
  evt_grep "$D37S/tasks/$id37e/events.log" notify-failed
tmx kill-window -t "$w_short" 2>/dev/null || true

else
  printf 'SKIP: 37 agy runtime（SRC_KIND=bash：bash 正本凍結在 M4，不支援 agy）\n'
fi

# ---- 38. list --long 介入視圖 ----
# spec: CLI-LIST-1 CLI-LIST-2
# 使用者痛點：`list` 只給 name/pane/ready，人要介入時看不出「pane 在哪、誰派的、
# 哪個能刪」。資料本來就在 registry，只是沒有指令一次講完。
#
# 本組鎖三件事：① 裸 `list` 一個位元組都沒變（既有腳本的介面）；② --long 的
# 失效降級是**顯式字面值**且三態分明（活著／查了不在／沒得查）；③ 唯讀——判定
# dead 不得順手清 registry。第 ② 條是這個功能唯一會騙人的地方：把 owner 已死
# 顯示成一個空位置，比不顯示更糟。
#
# 與 35–37 同理只對 Rust 執行；bash 正本自 M4 凍結，顯式 SKIP。
if [[ "$SRC_KIND" == rust ]]; then

D38="$TESTROOT/d38"
mkdir -p "$D38/agents"
# 38a 裸 list 不因 --long 的加入而改變：與改動前的三欄形逐字比對
p38="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D38" register w38 "$p38" >/dev/null 2>&1
ab "$D38" list > "$TESTROOT/l38-bare.txt" 2>/dev/null
# 精確比對整份輸出而非 grep 一條期望列：grep 擋不住「多一列／多一欄／插入
# 其他文字」，那些照樣是裸介面的行為變更（跨廠複核 2026-07-31 指出的假綠）
printf 'w38\t%s\t-\n' "$p38" > "$TESTROOT/l38-bare-want.txt"
assert "38a 裸 list 輸出與期望逐位元組相同（不只是含某一列）" \
  cmp -s "$TESTROOT/l38-bare-want.txt" "$TESTROOT/l38-bare.txt"

# 38b --long：標頭在第一行、欄數恰八
ab "$D38" list --long > "$TESTROOT/l38-long.txt" 2>/dev/null
assert "38b --long 首行為欄名標頭" \
  bash -c "head -1 '$TESTROOT/l38-long.txt' | grep -qFx 'NAME	PANE	READY	ORIGIN	WHERE	OWNER	DISPOSABLE	IDLE'"
assert "38b --long 每列恰八個 TAB 分隔欄" \
  bash -c "awk -F'\t' 'NF != 8 { bad = 1 } END { exit bad }' '$TESTROOT/l38-long.txt'"
ab "$D38" list -l > "$TESTROOT/l38-short.txt" 2>/dev/null
# 比 IDLE 以外的七欄：IDLE 是牆鐘秒數，兩次呼叫本來就可能差一秒——
# 拿它比對會做出一條會隨機紅的斷言（flake 比漏測更糟）
cut -f1-7 "$TESTROOT/l38-long.txt" > "$TESTROOT/l38-long7.txt"
cut -f1-7 "$TESTROOT/l38-short.txt" > "$TESTROOT/l38-short7.txt"
assert "38b -l 是 --long 的別名（除 IDLE 秒數外輸出相同）" \
  cmp -s "$TESTROOT/l38-long7.txt" "$TESTROOT/l38-short7.txt"

# 38c 人工註冊：origin=manual、owner 欄為 `-`（沒有 owner 概念，不是死了）
row38c="$(grep '^w38	' "$TESTROOT/l38-long.txt")"
assert "38c 人工註冊 origin 為 manual" \
  bash -c "printf '%s' '$row38c' | awk -F'\t' '\$4 == \"manual\" { exit 0 } { exit 1 }'"
assert "38c 人工註冊 owner 為 -（無 owner 概念，非 owner-dead）" \
  bash -c "printf '%s' '$row38c' | awk -F'\t' '\$6 == \"-\" { exit 0 } { exit 1 }'"
assert "38c 活著的 pane 位置解析為 <session>:<window>，非裸 %%id" \
  bash -c "printf '%s' '$row38c' | awk -F'\t' '\$5 ~ /^[^:]+:[0-9]+\$/ { exit 0 } { exit 1 }'"

# 38d pane 已死 → `dead`；且**唯讀**：registry 不得被順手清掉
tmx kill-pane -t "$p38" 2>/dev/null || true
# kill-pane 是同步的（tmux 回來時 pane 已不在 list-panes），不需等待
ab "$D38" list --long > "$TESTROOT/l38-dead.txt" 2>/dev/null
assert "38d pane 已死 → WHERE 欄為 dead" \
  bash -c "grep '^w38	' '$TESTROOT/l38-dead.txt' | awk -F'\t' '\$5 == \"dead\" { exit 0 } { exit 1 }'"
assert "38d 唯讀：判定 dead 不得清掉 registry 檔" test -f "$D38/agents/w38.json"

# 38e owner window 已死 → `owner-dead`。tmux 對不存在的 window id 會靜靜回
# `:` 且 exit 0（實測 3.7b），所以這條同時是「別用 exit code 判存在性」的回歸
D38E="$TESTROOT/d38e"
mkdir -p "$D38E/agents"
jq -n --arg p "$(tmx list-panes -t it -F '#{pane_id}' | head -1)" \
  '{name:"w38e", pane_id:$p, spawned:true, ready:true, owner:"nosuch:@99999", runtime:"claude"}' \
  > "$D38E/agents/w38e.json"
ab "$D38E" list --long > "$TESTROOT/l38-owner.txt" 2>/dev/null
assert "38e owner window 不存在 → OWNER 欄為 owner-dead（不是空位置）" \
  bash -c "grep '^w38e	' '$TESTROOT/l38-owner.txt' | awk -F'\t' '\$6 == \"owner-dead\" { exit 0 } { exit 1 }'"

# 38f tmux 不可用 → `?`（未知），**不得**標成 dead：那會讓整池看似該回收
env AGENT_BRIDGE_DATA="$D38E" PATH="$FAILSHIM:$PATH" "$BRIDGE" list --long \
  > "$TESTROOT/l38-notmux.txt" 2>/dev/null
assert "38f tmux 不可用 → WHERE 為 ?（未知，不是 dead）" \
  bash -c "grep '^w38e	' '$TESTROOT/l38-notmux.txt' | awk -F'\t' '\$5 == \"?\" { exit 0 } { exit 1 }'"
assert "38f tmux 不可用 → OWNER 為 ?（未知，不是 owner-dead）" \
  bash -c "grep '^w38e	' '$TESTROOT/l38-notmux.txt' | awk -F'\t' '\$6 == \"?\" { exit 0 } { exit 1 }'"

# 38g 損壞的 registry 不得終止整份報表：該列以 ? 呈現後繼續（它照樣佔著 cap）
printf 'not json at all\n' > "$D38E/agents/w38bad.json"
ab "$D38E" list --long > "$TESTROOT/l38-bad.txt" 2>/dev/null; rc=$?
assert "38g 損壞 registry：exit 仍 0" test "$rc" -eq 0
assert "38g 損壞 registry：整列七個狀態欄全為 ?、idle 為 -（不是只看兩欄）" \
  bash -c "grep '^w38bad	' '$TESTROOT/l38-bad.txt' | grep -qFx \"\$(printf 'w38bad\t?\t?\t?\t?\t?\t?\t-')\""
assert "38g 損壞 registry：其餘列照常輸出（沒有一壞全倒）" \
  grep -q '^w38e	' "$TESTROOT/l38-bad.txt"

# 38h 值域：「不得暗示可以安全刪除」是設計原則，不可機器判定全稱句——
# 用黑名單 regex 掃字面值是形式主義（RECLAIMABLE／SAFE／其他語言全繞得過，
# 跨廠複核 2026-07-31 指出）。可機器守的是**精確標頭＋各欄允許值域**：
# 任何新欄位或新狀態字都必須先改這裡，那才是真正的閘門
assert "38h 標頭逐字固定（新增欄位必須先改契約）" \
  bash -c "head -1 '$TESTROOT/l38-long.txt' | grep -qFx 'NAME	PANE	READY	ORIGIN	WHERE	OWNER	DISPOSABLE	IDLE'"
assert "38h READY 值域 ready|starting|-|?" \
  bash -c "tail -n +2 '$TESTROOT/l38-long.txt' | awk -F'\t' '\$3 !~ /^(ready|starting|-|\\?)\$/ { bad=1 } END { exit bad }'"
assert "38h ORIGIN 值域 spawned|manual|?" \
  bash -c "tail -n +2 '$TESTROOT/l38-long.txt' | awk -F'\t' '\$4 !~ /^(spawned|manual|\\?)\$/ { bad=1 } END { exit bad }'"
assert "38h DISPOSABLE 值域 yes|expired|-|?" \
  bash -c "tail -n +2 '$TESTROOT/l38-long.txt' | awk -F'\t' '\$7 !~ /^(yes|expired|-|\\?)\$/ { bad=1 } END { exit bad }'"
assert "38h IDLE 值域 非負整數|-" \
  bash -c "tail -n +2 '$TESTROOT/l38-long.txt' | awk -F'\t' '\$8 !~ /^([0-9]+|-)\$/ { bad=1 } END { exit bad }'"

# 38j registry 的 id 形狀壞掉 → invalid，**不是** dead：後者會讓人以為
# 「東西曾經在、現在沒了」，前者才是實情（資料損壞）
D38J="$TESTROOT/d38j"
mkdir -p "$D38J/agents"
jq -n '{name:"w38j", pane_id:"@garbage", spawned:true, ready:true, owner:"s:@nope", runtime:"claude"}' \
  > "$D38J/agents/w38j.json"
ab "$D38J" list --long > "$TESTROOT/l38-invalid.txt" 2>/dev/null
assert "38j pane_id 形狀不合法 → WHERE 為 invalid（非 dead）" \
  bash -c "grep '^w38j	' '$TESTROOT/l38-invalid.txt' | awk -F'\t' '\$5 == \"invalid\" { exit 0 } { exit 1 }'"
assert "38j owner 的 @id 形狀不合法 → OWNER 為 invalid（非 owner-dead）" \
  bash -c "grep '^w38j	' '$TESTROOT/l38-invalid.txt' | awk -F'\t' '\$6 == \"invalid\" { exit 0 } { exit 1 }'"

# 38k 欄值含 TAB／換行：合法 JSON string 塞一個 TAB 就能把一列變九欄，
# 破壞「一 agent 一行、恰八欄」的承諾
D38K="$TESTROOT/d38k"
mkdir -p "$D38K/agents"
jq -n '{name:"w38k\tINJECTED\tCOLS", pane_id:"%1", spawned:false}' > "$D38K/agents/w38k.json"
ab "$D38K" list --long > "$TESTROOT/l38-tab.txt" 2>/dev/null
assert "38k 欄值含 TAB 時每列仍恰八欄" \
  bash -c "awk -F'\t' 'NF != 8 { bad = 1 } END { exit bad }' '$TESTROOT/l38-tab.txt'"
assert "38k 註冊名裡的 TAB 被代換掉（不得原樣輸出）" \
  bash -c "! printf '%s' \"\$(tail -n +2 '$TESTROOT/l38-tab.txt')\" | grep -q 'w38k	INJECTED'"

# 38l linked window：tmux 的 window 可同時掛在多個 session，`-a` 列表因此
# 同 id 出現多次。取最後一筆＝隨列序給答案，而使用者問的正是「跟哪個主
# session 關聯」（跨廠複核 2026-07-31 的 major；用 HashMap 覆寫的版本這裡必紅）
D38L="$TESTROOT/d38l"
mkdir -p "$D38L/agents"
tmx new-session -d -s linkA -x 200 -y 100 "$pane_cmd"
w38l="$(tmx list-windows -t linkA -F '#{window_id}' | head -1)"
p38l="$(tmx list-panes -t "$w38l" -F '#{pane_id}' | head -1)"
tmx new-session -d -s linkB -x 200 -y 100 "$pane_cmd"
tmx link-window -s "$w38l" -t linkB: 2>"$TESTROOT/l38-link.err"
assert "38l 前置：link-window 成功（錯誤不得被吞）" \
  bash -c "test ! -s '$TESTROOT/l38-link.err'"
# 先把清單落檔再斷言：`tmx` 是本殼的 function，`bash -c` 子殼裡不存在，
# 直接寫進 assert 會讓前置永遠紅（同 37e 的教訓）
tmx list-panes -a -F '#{pane_id}' > "$TESTROOT/l38-panes.txt"
assert "38l 前置：同一 window 確實掛在兩個 session" \
  bash -c "test \"\$(grep -cx -- '$p38l' '$TESTROOT/l38-panes.txt')\" -eq 2"
# owner 記的是 linkB → 消歧義後 OWNER 應恰為 linkB 那一筆
jq -n --arg p "$p38l" --arg w "$w38l" \
  '{name:"w38l", pane_id:$p, spawned:true, ready:true, owner:("linkB:" + $w), runtime:"claude"}' \
  > "$D38L/agents/w38l.json"
ab "$D38L" list --long > "$TESTROOT/l38-linked.txt" 2>/dev/null
assert "38l WHERE 列出全部 live 位置（逗號分隔，不任選一個）" \
  bash -c "grep '^w38l	' '$TESTROOT/l38-linked.txt' | awk -F'\t' '\$5 ~ /^linkA:[0-9]+,linkB:[0-9]+\$/ { exit 0 } { exit 1 }'"
assert "38l OWNER 以 registry 的 session 標籤消歧義（恰為 linkB 那一筆）" \
  bash -c "grep '^w38l	' '$TESTROOT/l38-linked.txt' | awk -F'\t' '\$6 ~ /^linkB:[0-9]+\$/ { exit 0 } { exit 1 }'"
tmx kill-session -t linkA 2>/dev/null || true
tmx kill-session -t linkB 2>/dev/null || true

# 38i 參數面：未知參數要拒，不得靜默當成裸 list
assert_fails "38i list 未知參數應拒" ab "$D38" list --bogus
assert_fails "38i list 多餘參數應拒" ab "$D38" list --long extra

else
  printf 'SKIP: 38 list --long（SRC_KIND=bash：bash 正本凍結在 M4）\n'
fi

# ---- 39 copy-mode 送鍵防線（AB-COPYMODE-1）----
#
# 人一捲動 worker pane 就會進 tmux copy-mode——那正是「人為介入」情境本身。
# 實測：pane 在 copy-mode 時 `tmux send-keys -t <pane> 'agent-bridge receive
# <id>'` **永不返回**，於是 orchestrator 的整條 send 被鎖死，且沒有逾時能救。
#
# 本組鎖四件事：① copy-mode 中一個按鍵都不送、降級成 notify-failed；②
# **不得**替人 `-X cancel`——人正在看的捲動位置是介入現場，清掉比不通知更糟；
# ③ 送鍵子行程有逾時兜底（檢查與送鍵之間的 TOCTOU 空窗仍可能撞上 copy-mode，
# 沒有逾時那個空窗就是永久鎖死）；④ mode 查不出來時 fail-closed。
#
# 與 35–38 同理只對 Rust 執行；bash 正本自 M4 凍結，顯式 SKIP。
if [[ "$SRC_KIND" == rust ]]; then

D39="$TESTROOT/d39"

# 39a／39b 真 copy-mode。用一個**全透傳**的 shim（行為與正常 SHIM 相同，只多
# 記一筆 send-keys 呼叫）：光看「shell 沒收到字」證不了 gate 存在——拿掉 gate
# 只留逾時，send-keys 照樣被呼叫、照樣卡死逾時、shell 照樣沒收到字、mode 照樣
# 沒被取消，四條斷言全綠（跨廠複核 2026-07-31 finding 3 的假綠）。marker 檔把
# 「有沒有真的呼叫下去」直接變成可觀察量。
KEYSHIM="$TESTROOT/keyshim"
mkdir -p "$KEYSHIM"
cat > "$KEYSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
if [[ "\$1" == "send-keys" ]]; then
  { printf '%s ' "\$@"; printf '\n'; } >> $(printf '%q' "$TESTROOT/a39-sendkeys.log")
fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$KEYSHIM/tmux"

p39="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
ab "$D39" register cm39 "$p39" >/dev/null 2>&1
tmx send-keys -t "$p39" \
  "printf '%s\n' 'normal worker output' ; touch $TESTROOT/cm-ready ; while IFS= read -r l ; do printf '%s\n' \"\$l\" >> $TESTROOT/cm-got.txt ; done" Enter
wait_for 10 test -f "$TESTROOT/cm-ready"
tmx copy-mode -t "$p39"
# 前置條件：pane 真的進了 mode。不驗這條，39a 可能是在「根本沒進 copy-mode」
# 的畫面上假綠通過
tmx display -pt "$p39" '#{pane_in_mode}' > "$TESTROOT/cm-mode-pre.txt" 2>/dev/null
assert "39a 前置：pane 確實進入 copy-mode" \
  grep -Fxq 1 "$TESTROOT/cm-mode-pre.txt"
# 逾時值壓到 2 秒：防線若破了，這裡是套件唯一會撞到卡死的地方，不該讓它等滿
# 預設窗。整個呼叫再包一層 timeout，鎖住「有限時間內返回」本身
id39="$(env AGENT_BRIDGE_DATA="$D39" AGENT_BRIDGE_TMUX_TIMEOUT=2 PATH="$KEYSHIM:$PATH" \
  timeout 30 "$BRIDGE" send cm39 --from alice --message hi 2>/dev/null)"
assert "39a copy-mode：send 仍在有限時間內返回並產生任務 id" \
  test -n "$id39"
assert "39a copy-mode：send-keys 根本沒被呼叫（是 gate 擋下，不是逾時擋下）" \
  test ! -f "$TESTROOT/a39-sendkeys.log"
assert "39a copy-mode：events.log 記 notify-failed（降級而非靜默成功）" \
  evt_grep "$D39/tasks/$id39/events.log" notify-failed
assert_reason "39a copy-mode：notify-failed 標 reason=copy-mode" \
  "$D39/tasks/$id39/events.log" copy-mode
# 39b 先驗現場沒被清掉，再由測試自己離開 mode
tmx display -pt "$p39" '#{pane_in_mode}' > "$TESTROOT/cm-mode-post.txt" 2>/dev/null
assert "39b 不得替人 cancel：pane 仍停在 copy-mode（捲動現場保住）" \
  grep -Fxq 1 "$TESTROOT/cm-mode-post.txt"
tmx send-keys -X -t "$p39" cancel 2>/dev/null || true
# 哨兵法驗「一個按鍵都沒送」：離開 mode 後補一行哨兵，got 應恰好只有哨兵
tmx send-keys -t "$p39" 'SENTINEL-cm' Enter
assert "39a copy-mode：記錄機制活著（哨兵行有進 got）" \
  wait_for 10 grep -q 'SENTINEL-cm' "$TESTROOT/cm-got.txt"
# shellcheck disable=SC2016  # 單引號故意：$1/$2 由內層 bash 展開
assert "39a copy-mode：got 恰好只有哨兵一行（通知文字一個按鍵都沒送達）" \
  bash -c 'test "$(wc -l < "$1")" -eq 1 && grep -Fxq "$2" "$1"' _ "$TESTROOT/cm-got.txt" 'SENTINEL-cm'
tmx kill-pane -t "$p39" 2>/dev/null || true

# 39c 逾時兜底：send-keys 永不返回時，指令 MUST NOT 被無限鎖死。
# 真 copy-mode 的卡死不是每個 tmux 版本／mode-keys 設定都構造得出來，改用
# shim 直接注入「send-keys 掛住」這個終態——鎖的是逾時機制本身。
# `exec sleep` 而非 `sleep`：被殺的必須是 sleep 本人，否則 shim 死了、孫行程
# 還在背景睡滿 300 秒。
D39c="$TESTROOT/d39c"
HANGSHIM="$TESTROOT/hangshim"
mkdir -p "$HANGSHIM"
cat > "$HANGSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
if [[ "\$1" == "send-keys" ]]; then
  touch $(printf '%q' "$TESTROOT/c39-hang-hit")
  exec sleep 300
fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$HANGSHIM/tmux"
env AGENT_BRIDGE_DATA="$D39c" PATH="$HANGSHIM:$PATH" "$BRIDGE" \
  register hang39 "$PANE_B" >/dev/null 2>&1
id39c="$(env AGENT_BRIDGE_DATA="$D39c" AGENT_BRIDGE_TMUX_TIMEOUT=2 PATH="$HANGSHIM:$PATH" \
  timeout 30 "$BRIDGE" send hang39 --from alice --message hi 2>/dev/null)"
assert "39c 逾時兜底：send-keys 卡死時指令仍在有限時間內返回" \
  test -n "$id39c"
# marker 排除「PATH 沒生效、其實走了正常路徑」的偽陰性
assert "39c 逾時兜底：shim 的 send-keys 確實被呼叫到" \
  test -f "$TESTROOT/c39-hang-hit"
assert "39c 逾時兜底：events.log 記 notify-failed" \
  evt_grep "$D39c/tasks/$id39c/events.log" notify-failed
# 這是唯一走得到 send-keys-failed 桶的真實佈景（前兩道關卡都過、卡在送鍵本身）
assert_reason "39c 逾時兜底：notify-failed 標 reason=send-keys-failed" \
  "$D39c/tasks/$id39c/events.log" send-keys-failed

# 39d mode 查不出來 → fail-closed。只讓 `#{pane_in_mode}` 查詢失敗，其餘子命令
# 照常透傳：無法確認 pane 狀態時放行送鍵，等於整條防線被略過。
D39d="$TESTROOT/d39d"
MODESHIM="$TESTROOT/modeshim"
mkdir -p "$MODESHIM"
cat > "$MODESHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
for a in "\$@"; do
  if [[ "\$a" == '#{pane_in_mode}' ]]; then
    touch $(printf '%q' "$TESTROOT/d39-mode-hit")
    echo "display failed (test stub)" >&2; exit 1
  fi
done
if [[ "\$1" == "send-keys" ]]; then
  { printf '%s ' "\$@"; printf '\n'; } >> $(printf '%q' "$TESTROOT/d39-sendkeys.log")
  exit 0
fi
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$MODESHIM/tmux"
env AGENT_BRIDGE_DATA="$D39d" PATH="$MODESHIM:$PATH" "$BRIDGE" \
  register mode39 "$PANE_B" >/dev/null 2>&1
id39d="$(env AGENT_BRIDGE_DATA="$D39d" PATH="$MODESHIM:$PATH" \
  timeout 30 "$BRIDGE" send mode39 --from alice --message hi 2>/dev/null)"
assert "39d fail-closed：mode 查詢確實被 shim 攔截" \
  test -f "$TESTROOT/d39-mode-hit"
assert "39d fail-closed：mode 查不出來時一個按鍵都不送" \
  test ! -f "$TESTROOT/d39-sendkeys.log"
assert "39d fail-closed：events.log 記 notify-failed" \
  evt_grep "$D39d/tasks/$id39d/events.log" notify-failed
# mode 查不出來歸 pane-gone 桶（「pane 狀態查不到」與「pane 真的不在」同處置）
assert_reason "39d fail-closed：notify-failed 標 reason=pane-gone" \
  "$D39d/tasks/$id39d/events.log" pane-gone

# 39e 逾時上限涵蓋整條通知路徑，不只 send-keys：notify_pane 在送鍵之前還會跑
# list-panes／display／capture-pane 三種查詢，任何一個卡住，send 照樣無限等待
# ——只擋 send-keys 等於上限形同不存在（跨廠複核 2026-07-31 finding 1）。
# 這裡讓 mode 查詢永不返回：它排在 send-keys 之前，逾時若沒套到查詢層就會鎖死。
D39e="$TESTROOT/d39e"
MODEHANGSHIM="$TESTROOT/modehangshim"
mkdir -p "$MODEHANGSHIM"
cat > "$MODEHANGSHIM/tmux" <<EOF
#!/usr/bin/env bash
unset TMUX
for a in "\$@"; do
  if [[ "\$a" == '#{pane_in_mode}' ]]; then
    touch $(printf '%q' "$TESTROOT/e39-modehang-hit")
    exec sleep 300
  fi
done
exec $(printf '%q' "$REAL_TMUX") -L $(printf '%q' "$SOCK") -f /dev/null "\$@"
EOF
chmod +x "$MODEHANGSHIM/tmux"
env AGENT_BRIDGE_DATA="$D39e" PATH="$MODEHANGSHIM:$PATH" "$BRIDGE" \
  register modehang39 "$PANE_B" >/dev/null 2>&1
id39e="$(env AGENT_BRIDGE_DATA="$D39e" AGENT_BRIDGE_TMUX_TIMEOUT=2 PATH="$MODEHANGSHIM:$PATH" \
  timeout 30 "$BRIDGE" send modehang39 --from alice --message hi 2>/dev/null)"
assert "39e 查詢層逾時：mode 查詢卡死時指令仍在有限時間內返回" \
  test -n "$id39e"
assert "39e 查詢層逾時：shim 的 mode 查詢確實被呼叫到" \
  test -f "$TESTROOT/e39-modehang-hit"
assert "39e 查詢層逾時：events.log 記 notify-failed" \
  evt_grep "$D39e/tasks/$id39e/events.log" notify-failed
# mode 查詢卡到逾時＝查不出狀態，同樣落在 pane-gone 桶（不是 send-keys-failed
# ——那時根本還沒走到送鍵；桶分錯會讓事後分析把關卡歸錯）
assert_reason "39e 查詢層逾時：notify-failed 標 reason=pane-gone" \
  "$D39e/tasks/$id39e/events.log" pane-gone

else
  printf 'SKIP: 39 copy-mode 送鍵防線（SRC_KIND=bash：bash 正本凍結在 M4）\n'
fi

# ---- 40. TUI 第一縱切（ui dashboard） ----
# spec: CLI-UI-1 CLI-CANCEL-1
# 設計正本 docs/tui-design.md §8／§9 P1 的三個 gate，全部腳本化：
#   (a) 導航到指定 task 列，x＋確認 → task 檔狀態轉 cancelled（且確認框逐字
#       顯示等價 CLI 原文；通知事件落地；別的 task 不受波及）
#   (b) 退出後 window id／layout string／geometry 不變、**termios 逐字還原**
#       （同一條 shell 命令內 `stty -g` 前後比對——bash/readline 重畫 prompt
#       時會自行重設 termios，隔著 prompt 比對驗不到 raw mode 有沒有留下）；
#       正常退出（q）／panic／非 panic 的 Err 三條路徑各一次
#   (c) Enter 後**真 client**（control-mode attached client）的
#       `session:window.pane` 等於目標——不是 detached session 的 active
#       window：後者在 switch/select 全壞掉時照樣綠
# 另含消費端防線：無界 tmux（hanging shim＋TMUX_TIMEOUT=0）下 UI 仍畫得出來、
# 收得了鍵（§4 bounded-read 硬條款的 UI 側），以及 popup 啟動器協定
# （AGENT_BRIDGE_UI_POPUP=1 時 focus 成功即正常退出，ENV-UI-1）。
# 畫面斷言一律 capture-pane 落檔後 grep 特徵字串，不做整畫面 byte 比對
# （alternate screen 承諾不了 byte 級不變，終審已裁定）。
# 與 35–39 同理只對 Rust 執行；bash 正本自 M4 凍結，顯式 SKIP。
if [[ "$SRC_KIND" == rust ]]; then

D40="$TESTROOT/d40"
mkdir -p "$D40/agents" "$D40/tasks"

# fixture：2 owner／3 worker／2 task（P1 用小 fixture；P4 的大 fixture 不在本階段）
# live worker 放在獨立 window：Enter focus 的跨 window 案例
W40_WIN="$(tmx new-window -dP -F '#{window_id}' -t it "$pane_cmd")"
P40A="$(tmx list-panes -t "$W40_WIN" -F '#{pane_id}')"
# dead worker：開了就殺（驗 dead 標示不毒死畫面；也是 P1 之後異常軸的地基）
P40B="$(tmx split-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
tmx kill-pane -t "$P40B"
# TUI 跑在自己 window 的 bash 裡（不是直接當 window command）：退出後 pane
# 仍在，才驗得了「退出還原、terminal 可用」
TUI40="$(tmx new-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
# current origin＝TUI 自己所在的 window（§2「以 current origin 為根」）
OWN40="$(tmx display -p -t "$TUI40" '#{session_name}:#{window_id}')"
SESS40="${OWN40%%:*}"
# P4.6 題 1：ORIGINS 欄顯示的是 window **名稱**。給它一個固定名字，斷言才有
# 可預期的字面（預設名是啟動命令，會隨 $pane_cmd 漂移）
WIN40NAME=ab40
tmx rename-window -t "$TUI40" "$WIN40NAME"
# 第二個 origin，字典序**刻意排在 current 之前**（`aaa:` < `it:`）：首頁若退
# 回第 0 筆（P4.6 之後是 synthetic `ALL`），這一列的 worker 就會出現在 WORKERS
# 欄 → F2 回歸抓得到。window id 取一個**確定不存在**的值：P4.6 之後 origin 列
# 顯示的是 `session:window-name`，只有已消失的 window 才會原樣留著 `@id`，
# 這一列的字面才在 tmux 活著／掛住兩種情況下都可預期
OWN40X="aaa:@94001"
printf '{"name":"tuiw1","pane_id":"%s","registered_at":"2026-07-31T00:00:00Z","spawned":true,"ready":true,"runtime":"codex","spawn_tag":"t40-1","owner":"%s"}\n' \
  "$P40A" "$OWN40" > "$D40/agents/tuiw1.json"
printf '{"name":"tuiw2","pane_id":"%s","registered_at":"2026-07-31T00:00:00Z","spawned":true,"ready":true,"runtime":"claude","spawn_tag":"t40-2","owner":"%s"}\n' \
  "$P40B" "$OWN40" > "$D40/agents/tuiw2.json"
printf '{"name":"tuiwother","pane_id":"%%940","registered_at":"2026-07-31T00:00:00Z","spawned":true,"ready":true,"runtime":"codex","spawn_tag":"t40-3","owner":"%s"}\n' \
  "$OWN40X" > "$D40/agents/tuiwother.json"
# 兩個 queued task 都派給 tuiw1（手工鋪完整形狀：metadata＋status＋request，
# 不經 send——不觸發通知，狀態確定停在 queued）
T40A="20260731T000001Z-40aa"
T40B="20260731T000002Z-40bb"
for _t40 in "$T40A" "$T40B"; do
  mkdir -p "$D40/tasks/$_t40"
  printf 'queued\n' > "$D40/tasks/$_t40/status"
  printf 'p1 fixture task\n' > "$D40/tasks/$_t40/request.md"
  printf '{"version":1,"task_id":"%s","from":"alice","to":"tuiw1","created_at":"2026-07-31T00:00:01Z","updated_at":"2026-07-31T00:00:01Z","working_directory":"/tmp","status":"queued"}\n' \
    "$_t40" > "$D40/tasks/$_t40/metadata.json"
done

tmx send-keys -t "$TUI40" \
  "$(printf 'echo AB40-MAIN-SCREEN; touch %q' "$TESTROOT/tui40-ready")" Enter
wait_for 10 test -f "$TESTROOT/tui40-ready"

# gate (c) 要的是**真 client**：整套測試的 tmux server 全程 detached，
# `display -t it:` 只讀得到 session 的 active window——switch-client／
# select-pane 全壞掉它照樣綠。這裡起一個 control-mode client（不需要 pty、
# 不改動 window 尺寸與 layout，已實測），之後一律以 `display -c <client>`
# 觀測「使用者實際在哪」。fd 9 常開著 fifo，client 才不會在下一條命令就掉。
CC40="$TESTROOT/tui40-cc.fifo"
mkfifo "$CC40"
( tmx -C attach -t it < "$CC40" > "$TESTROOT/tui40-cc.log" 2>&1 & )
exec 9<>"$CC40"
# shellcheck disable=SC2329  # 經 wait_for 間接呼叫
cc40_up() { [[ -n "$(tmx list-clients -F '#{client_name}' 2>/dev/null)" ]]; }
assert "40 前置：control-mode client 已 attach（gate (c) 的觀測點）" \
  wait_for 10 cc40_up
CLIENT40="$(tmx list-clients -F '#{client_name}' 2>/dev/null | head -1)"

# fixture 齊備後快照 tmux 世界（gate (b) 的比對基準）：window id＋layout
# string（內含 geometry 與 pane id）
tmx list-windows -F '#{window_id} #{window_layout}' > "$TESTROOT/tui40-before.txt"

# tui40_run <tag> [env 指派…]：在 TUI40 的 shell 裡跑一次 `ui`，並以**同一條
# shell 命令**把 `stty -g` 夾在前後——中間不回 prompt，bash/readline 沒有機會
# 代為重設 termios，raw mode 有沒有還原才驗得準（審查 F5）。
tui40_run() {
  local tag="$1"; shift
  tmx send-keys -t "$TUI40" "$(printf 'stty -g > %q; env %s AGENT_BRIDGE_DATA=%q %q ui; stty -g > %q' \
    "$TESTROOT/stty-$tag-before" "$*" "$D40" "$BRIDGE" "$TESTROOT/stty-$tag-after")" Enter
}
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
stty40_same() { diff -q "$TESTROOT/stty-$1-before" "$TESTROOT/stty-$1-after"; }

# tmx 是本檔的 shell function，bash -c 子殼裡不存在：一律先擷取到檔案再斷言
cap40() { tmx capture-pane -p -t "$TUI40" > "$TESTROOT/tui40-cap.txt" 2>/dev/null; }
# shellcheck disable=SC2329  # 經 assert/wait_for 的 "$@" 間接呼叫
ui_shows() { cap40 && grep -qF "$1" "$TESTROOT/tui40-cap.txt"; }
# shellcheck disable=SC2329  # 經 assert/wait_for 的 "$@" 間接呼叫
ui_lacks() { cap40 && ! grep -qF "$1" "$TESTROOT/tui40-cap.txt"; }
# 真 client 當下所在的 session:window.pane（不是 session 的 active window）
# shellcheck disable=SC2329  # 經 wait_for 間接呼叫
focus40_is() {
  [[ "$(tmx display -p -c "$CLIENT40" '#{session_name}:#{window_id}.#{pane_id}' 2>/dev/null)" \
     == "it:$W40_WIN.$P40A" ]]
}

tui40_run q
# 40a 讀模型→畫面：owner／兩 worker／兩 task 列（權威狀態字）都在
assert "40a UI 起畫面：worker 列 tuiw1 可見" wait_for 10 ui_shows "tuiw1"
assert "40a UI：dead worker tuiw2 亦在列（不毒死畫面）" ui_shows "tuiw2"
# P4.6 題 1／題 2：ORIGINS 欄顯示 `session:window-name`（不是 `session:@id`），
# 已消失的 window 才保留原 `@id` 並標 `(gone)`——**不猜舊名**
# **要等**：origin 標籤的誠實化吃的是 liveness 那一輪（2s 節流、走背景
# worker），磁碟 read model 先到、window 索引後到——不等就是在驗第一幀的
# unknown 降級態
assert "40a UI：current origin 以 session:window-name 顯示（非 @id）" \
  wait_for 10 ui_shows "$SESS40:$WIN40NAME"
assert "40a UI：origin 列不再出現 window id（誠實標籤，題 1）" ui_lacks "$OWN40"
assert "40a UI：已消失 window 的 origin 標 (gone)、且不猜舊名" \
  ui_shows "$OWN40X (gone)"
assert "40a UI：ORIGINS 欄頂端有 synthetic ALL 列（題 2）" ui_shows "ALL"
# §2「以 current owner 為根」：字典序第 0 筆是 aaa:@1，首頁若退字典序就會顯示
# 它底下的 tuiwother——WORKERS 欄看不到它，才證明根是 current owner（審查 F2）
assert "40a 首頁根＝current owner（非字典序第一筆）" wait_for 10 ui_lacks "tuiwother"
assert "40a UI：task 列帶 immutable id（$T40A）" ui_shows "$T40A"
assert "40a UI：第二個 task 列亦可見" ui_shows "$T40B"
assert "40a UI：task status 顯示權威字 queued" ui_shows "queued"
# alternate screen 進場：主畫面的哨兵行此刻不可見
cap40
assert "40a alternate screen：主畫面內容已被覆蓋" \
  bash -c '! grep -qF AB40-MAIN-SCREEN "$1"' _ "$TESTROOT/tui40-cap.txt"

# 40b ?＝當前選中項的合法鍵；任意鍵關閉
tmx send-keys -t "$TUI40" '?'
assert "40b ? 顯示當前選中項合法鍵（chrome 全英文，題 9）" \
  wait_for 10 ui_shows "keys (current selection)"
tmx send-keys -t "$TUI40" k

# 40c x 的合法目標只有 task 列：worker 列上按 x 無效並提示
tmx send-keys -t "$TUI40" x
assert "40c worker 列按 x：提示無效、不開確認框" \
  wait_for 10 ui_shows "x only acts on task rows"

# 40d gate (c)：Enter focus 跨 window。前置：真 client 不在目標 window 上
tmx display -p -c "$CLIENT40" '#{window_id}' > "$TESTROOT/tui40-curwin.txt"
assert "40d 前置：client 不在目標 window（跨 window 案例成立）" \
  bash -c '! grep -qFx "$1" "$2"' _ "$W40_WIN" "$TESTROOT/tui40-curwin.txt"
tmx send-keys -t "$TUI40" Enter
assert "40d Enter focus：真 client 的 session:window.pane 等於目標" \
  wait_for 10 focus40_is
# focus 走背景 worker（審查 F1）：結果經 channel 回到 footer，且**UI 沒退出**
# ——非 popup 模式 focus 後繼續跑（與 40j 的 popup 協定成對）
assert "40d focus 結果回到 footer（背景 worker → channel）" \
  wait_for 10 ui_shows "focused 'tuiw1'"

# 40e gate (a)：導航到第一個 task 列，x → 確認框顯示等價 CLI 原文 → y 執行
tmx send-keys -t "$TUI40" j
tmx send-keys -t "$TUI40" x
assert "40e 確認框逐字顯示等價 CLI：agent-bridge cancel <task-id>" \
  wait_for 10 ui_shows "agent-bridge cancel $T40A"
tmx send-keys -t "$TUI40" y
assert "40e 確認後 task 檔狀態轉 cancelled" wait_for 10 st_is "$D40" "$T40A" cancelled
assert "40e events.log 記 cancelled 事件" evt_grep "$D40/tasks/$T40A/events.log" cancelled
# CLI-UI-1 要求「與 cancel 相同的轉換**與通知**」：少了通知呼叫，狀態照樣轉
# cancelled，只有通知事件會消失——故必須另立斷言（審查 F6）
# 通知在轉態**之後**、且送鍵之間隔 NOTIFY_DELAY，故要等（狀態轉好不代表
# 通知已落地）
assert "40e 通知語意與 CLI 一致（notified／notify-deferred／notify-failed 之一）" \
  wait_for 10 evt_grep "$D40/tasks/$T40A/events.log" '(notified|notify-deferred|notify-failed)'
assert "40e cancel 結果回到 footer（背景 worker → channel，不印 stderr）" \
  wait_for 10 ui_shows "cancelled task $T40A"
assert "40e 另一個 task 不受波及（仍 queued）" st_is "$D40" "$T40B" queued

# 40f gate (b) 正常退出：q 後回主畫面、termios 逐字還原、tmux 世界不變
tmx send-keys -t "$TUI40" q
assert "40f q 退出：離開 alternate screen（主畫面哨兵行回來）" \
  wait_for 10 ui_shows "AB40-MAIN-SCREEN"
assert "40f q 退出：termios 逐字還原（同一條命令內 stty -g 前後比對）" \
  wait_for 10 stty40_same q
tmx send-keys -t "$TUI40" \
  "$(printf 'touch %q' "$TESTROOT/tui40-after-q")" Enter
assert "40f q 退出後 terminal 可用（shell 收得到指令）" \
  wait_for 10 test -f "$TESTROOT/tui40-after-q"
tmx list-windows -F '#{window_id} #{window_layout}' > "$TESTROOT/tui40-after.txt"
assert "40f window id／layout／geometry 不變" \
  diff -q "$TESTROOT/tui40-before.txt" "$TESTROOT/tui40-after.txt"

# 40g gate (b) 錯誤退出之一：panic 注入（AB_TUI_SELFTEST_PANIC——測試 harness
# 的注入點，非 AGENT_BRIDGE_* 設定面）。panic hook 先還原 terminal 再印訊息，
# 所以 panic 訊息必須出現在**主畫面**上
tui40_run panic AB_TUI_SELFTEST_PANIC=1
assert "40g panic 退出：訊息印在主畫面（alt screen 已離開＝hook 有跑）" \
  wait_for 10 ui_shows "AB_TUI_SELFTEST_PANIC"
assert "40g panic 退出：termios 逐字還原" wait_for 10 stty40_same panic
tmx send-keys -t "$TUI40" \
  "$(printf 'touch %q' "$TESTROOT/tui40-after-panic")" Enter
assert "40g panic 退出後 terminal 可用（raw mode 已還原）" \
  wait_for 10 test -f "$TESTROOT/tui40-after-panic"
tmx list-windows -F '#{window_id} #{window_layout}' > "$TESTROOT/tui40-after2.txt"
assert "40g panic 退出後 window id／layout／geometry 仍不變" \
  diff -q "$TESTROOT/tui40-before.txt" "$TESTROOT/tui40-after2.txt"

# 40h gate (b) 錯誤退出之二：**非 panic 的 Err**（AB_TUI_SELFTEST_ERR）。
# draw／event::poll／event::read 都以 Err 返回，那條路徑不經 panic hook；
# 只驗 panic 擋不住未來有人在 cleanup 之前提早 return（審查 F10）。
tui40_run err AB_TUI_SELFTEST_ERR=1
assert "40h Err 退出：錯誤訊息印在主畫面（已離開 alt screen）" \
  wait_for 10 ui_shows "AB_TUI_SELFTEST_ERR"
assert "40h Err 退出：termios 逐字還原" wait_for 10 stty40_same err
tmx list-windows -F '#{window_id} #{window_layout}' > "$TESTROOT/tui40-after3.txt"
assert "40h Err 退出後 window id／layout／geometry 仍不變" \
  diff -q "$TESTROOT/tui40-before.txt" "$TESTROOT/tui40-after3.txt"

# 40i §4 bounded-read 硬條款的 UI 側：tmux 整個掛住（hanging shim）＋
# AGENT_BRIDGE_TMUX_TIMEOUT=0（不設限）。tmux 全部在背景 worker 上跑，
# 所以 UI 仍須畫得出磁碟 read model、仍須收得了鍵、仍須 q 得掉（審查 F1）。
HANG40="$TESTROOT/hangshim40"
mkdir -p "$HANG40"
printf '#!/usr/bin/env bash\nsleep 30\n' > "$HANG40/tmux"
chmod +x "$HANG40/tmux"
tui40_run hang "AGENT_BRIDGE_TMUX_TIMEOUT=0 PATH=$(printf '%q' "$HANG40:$PATH")"
# 注意：tmux 掛住時連 current owner 都查不到，首頁只能退字典序第 0 筆
# （$OWN40X）——這正是「該欄降級、UI 照跑」的正確終態，故斷言挑
# origin-independent 的證據：ORIGINS 欄仍列得出磁碟上的 origin 標籤
# tmux 掛住時 window 索引整層 unknown：origin 標籤 MUST 退回原樣的
# `session:@id`，且 **MUST NOT** 標成 `(gone)`（查不到 ≠ 不在，§5 三態）
assert "40i 無界 tmux 下 UI 照樣畫得出磁碟 read model" \
  wait_for 10 ui_shows "$OWN40"
assert "40i 無界 tmux 下 liveness 降級（不凍結、照畫）" ui_shows "$OWN40X"
assert "40i 無界 tmux 下 unknown MUST NOT 被寫成 gone" ui_lacks "(gone)"
tmx send-keys -t "$TUI40" '?'
assert "40i 無界 tmux 下鍵盤仍活（? 開得了合法鍵頁）" \
  wait_for 10 ui_shows "keys (current selection)"
tmx send-keys -t "$TUI40" k
tmx send-keys -t "$TUI40" q
# 退出證據用檔案不用畫面：畫面只有可見區，連跑多輪後哨兵行會捲出去
assert "40i 無界 tmux 下 q 退得掉（UI thread 沒被 tmux 卡住）" \
  wait_for 10 test -f "$TESTROOT/stty-hang-after"
assert "40i 無界 tmux 退出後 termios 仍逐字還原" wait_for 10 stty40_same hang

# 40j 啟動器協定（ENV-UI-1）：AGENT_BRIDGE_UI_POPUP=1 時 focus 成功即正常
# 退出（`display-popup -E` 的行程結束＝關 popup，人落在目標 pane）。
# 前置：先把 client 帶離目標 window，否則「有沒有 focus」分不出來。
tmx select-window -t "$TUI40"
tui40_run popup AGENT_BRIDGE_UI_POPUP=1
assert "40j popup 模式：UI 起得來" wait_for 10 ui_shows "tuiw1"
tmx send-keys -t "$TUI40" Enter
assert "40j popup 模式：focus 成功後直接正常退出" \
  wait_for 10 test -f "$TESTROOT/stty-popup-after"
assert "40j popup 退出後真 client 落在目標 pane" wait_for 10 focus40_is
assert "40j popup 退出：termios 逐字還原" wait_for 10 stty40_same popup

# 收場：先放掉 control-mode client（fd 9 一關，attach 就結束），再殺本組開的
# window（P40B 已死）
exec 9>&-
tmx detach-client -t "$CLIENT40" 2>/dev/null || true
tmx kill-pane -t "$TUI40" 2>/dev/null || true
tmx kill-window -t "$W40_WIN" 2>/dev/null || true

else
  printf 'SKIP: 40 TUI 第一縱切（SRC_KIND=bash：bash 正本凍結在 M4，不含 TUI）\n'
fi

# ---- 41. TUI 四面板＋唯讀鍵（r／i／c） ----
# spec: CLI-UI-1 CLI-READ-1
# 設計正本 docs/tui-design.md §2 版面（四面板）與 §9 P2 的兩個 gate：
#   (a) `c` 後 `tmux show-buffer` 的內容含 immutable 證據與唯讀命令原文，且
#       **不含任何 mutation 子指令字串**（action 層另以假 Clipboard 斷言組裝，
#       見 `ab_tui::action::tests::copy_payload_is_read_only_evidence_for_every_selection`）
#   (b) `r` 的畫面側：`agent-bridge read <id>` 的 stdout 落檔，TUI 按 `r` 後
#       capture-pane 落檔，斷言 response 的特徵行出現在畫面上。**render 層
#       只做特徵字串比對**——alternate screen 承諾不了 byte 級不變（終審已
#       裁定）；byte 級由 `ab_core::task::tests::read_response_returns_verbatim_bytes_and_logs_read_event`
#       負責
# 另含：Tab 三欄循環、TASKS 欄含終態、終態列 `x` 被拒、`i` 摘要頁、
# 退出後 window id／layout／geometry 不變與 termios 逐字還原（沿用 40 的
# tui40_run／stty40_same 手法）。與 35–40 同理只對 Rust 執行。
if [[ "$SRC_KIND" == rust ]]; then

D41="$TESTROOT/d41"
mkdir -p "$D41/agents" "$D41/tasks"

# fixture：1 owner／1 live worker／2 task（1 completed＋1 queued——`r` 讀得動
# 的只有終態任務，這正是 TASKS 欄存在的理由）
W41_WIN="$(tmx new-window -dP -F '#{window_id}' -t it "$pane_cmd")"
P41A="$(tmx list-panes -t "$W41_WIN" -F '#{pane_id}')"
TUI41="$(tmx new-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
OWN41="$(tmx display -p -t "$TUI41" '#{session_name}:#{window_id}')"
SESS41="${OWN41%%:*}"
# P4.6 題 1：origin 列顯示 window 名稱，給它固定名字才驗得準
WIN41NAME=ab41
tmx rename-window -t "$TUI41" "$WIN41NAME"
printf '{"name":"tuiw41","pane_id":"%s","registered_at":"2026-07-31T04:41:00Z","spawned":true,"ready":true,"runtime":"codex","spawn_tag":"t41-gen1","owner":"%s"}\n' \
  "$P41A" "$OWN41" > "$D41/agents/tuiw41.json"

# 反序（新的在上）：completed 的 id 刻意比 queued 新，TASKS 欄第一列即可讀
T41DONE="20260731T004142Z-41dd"
# 後綴 MUST 是 hex：`is_generated_task_dirname` 只認 [0-9a-f]，非 hex 的目錄
# 一律被當成「不是本工具生成的」而跳過，read model 裡根本不會出現
T41Q="20260731T004141Z-41cc"
mkdir -p "$D41/tasks/$T41DONE" "$D41/tasks/$T41Q"
printf 'completed\n' > "$D41/tasks/$T41DONE/status"
printf 'p2 fixture task\n' > "$D41/tasks/$T41DONE/request.md"
printf 'AB41-RESPONSE-LINE 回覆全文的特徵行\n第二行\n' > "$D41/tasks/$T41DONE/response.md"
printf '{"version":1,"task_id":"%s","from":"alice","to":"tuiw41","created_at":"2026-07-31T04:41:42Z","updated_at":"2026-07-31T04:41:42Z","working_directory":"/tmp","status":"completed"}\n' \
  "$T41DONE" > "$D41/tasks/$T41DONE/metadata.json"
printf 'queued\n' > "$D41/tasks/$T41Q/status"
printf 'p2 fixture task\n' > "$D41/tasks/$T41Q/request.md"
printf '{"version":1,"task_id":"%s","from":"alice","to":"tuiw41","created_at":"2026-07-31T04:41:41Z","updated_at":"2026-07-31T04:41:41Z","working_directory":"/tmp","status":"queued"}\n' \
  "$T41Q" > "$D41/tasks/$T41Q/metadata.json"

# gate (b) 的比對來源：CLI 側的 stdout 先落檔（同時證明下沉 ab-core 之後
# CLI 行為不變，CLI-READ-1）
ab "$D41" read "$T41DONE" \
  > "$TESTROOT/t41-cli-read.out" 2> "$TESTROOT/t41-cli-read.err" || true
assert "41 CLI read stdout 恰為 response 內容（下沉 ab-core 後行為不變）" \
  diff -q "$D41/tasks/$T41DONE/response.md" "$TESTROOT/t41-cli-read.out"
assert "41 CLI read 標頭走 stderr（task-id/from/to）" \
  grep -q "^task-id: $T41DONE\$" "$TESTROOT/t41-cli-read.err"

# 下沉 ab-core 後最容易漂掉的是**呈現順序**：舊行為是「先印三行標頭到 stderr，
# 再串流 payload」，兩者都在 task 鎖內。若改成外殼拿到完整 outcome 才印，
# response.md 缺檔時就一行標頭都不印——可觀察的行為改變（跨廠審查 major #1）
T41NOR="20260731T004143Z-41ee"
mkdir -p "$D41/tasks/$T41NOR"
printf 'completed\n' > "$D41/tasks/$T41NOR/status"
printf '{"version":1,"task_id":"%s","from":"alice","to":"tuiw41","created_at":"2026-07-31T04:41:43Z","updated_at":"2026-07-31T04:41:43Z","working_directory":"/tmp","status":"completed"}\n' \
  "$T41NOR" > "$D41/tasks/$T41NOR/metadata.json"
ab "$D41" read "$T41NOR" \
  > "$TESTROOT/t41-nores.out" 2> "$TESTROOT/t41-nores.err" && rc41=0 || rc41=$?
assert "41 response.md 缺檔：read 以非零收場" test "$rc41" -ne 0
assert "41 response.md 缺檔：三行標頭仍先印出（順序不因下沉而改變）" \
  grep -q "^to: tuiw41\$" "$TESTROOT/t41-nores.err"
assert "41 response.md 缺檔：stdout 為空（payload 一個位元組都不該出）" \
  test ! -s "$TESTROOT/t41-nores.out"
rm -r "$D41/tasks/$T41NOR"

tmx send-keys -t "$TUI41" \
  "$(printf 'echo AB41-MAIN-SCREEN; touch %q' "$TESTROOT/tui41-ready")" Enter
wait_for 10 test -f "$TESTROOT/tui41-ready"
tmx list-windows -F '#{window_id} #{window_layout}' > "$TESTROOT/tui41-before.txt"

# 沿用 40 的手法：同一條 shell 命令內把 stty -g 夾在前後（中間不回 prompt）
tui41_run() {
  local tag="$1"; shift
  tmx send-keys -t "$TUI41" "$(printf 'stty -g > %q; env %s AGENT_BRIDGE_DATA=%q %q ui; stty -g > %q' \
    "$TESTROOT/stty41-$tag-before" "$*" "$D41" "$BRIDGE" "$TESTROOT/stty41-$tag-after")" Enter
}
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
stty41_same() { diff -q "$TESTROOT/stty41-$1-before" "$TESTROOT/stty41-$1-after"; }
# tmx 是本檔的 shell function，bash -c 子殼裡不存在：一律先擷取到檔案再斷言
cap41() { tmx capture-pane -p -t "$TUI41" > "$TESTROOT/tui41-cap.txt" 2>/dev/null; }
# shellcheck disable=SC2329  # 經 assert/wait_for 的 "$@" 間接呼叫
ui41_shows() { cap41 && grep -qF "$1" "$TESTROOT/tui41-cap.txt"; }
# shellcheck disable=SC2329  # 經 assert/wait_for 的 "$@" 間接呼叫
ui41_lacks() { cap41 && ! grep -qF "$1" "$TESTROOT/tui41-cap.txt"; }
# 面板聚焦只以文字 marker（`▶`）觀測：capture-pane 吃不到 style
# shellcheck disable=SC2329  # 經 assert/wait_for 的 "$@" 間接呼叫
ui41_matches() { cap41 && grep -qE "$1" "$TESTROOT/tui41-cap.txt"; }
# `r` 是否真的又記了一筆 read 事件（CLI 側已記過一筆，故門檻是 2）
# shellcheck disable=SC2329  # 經 assert/wait_for 的 "$@" 間接呼叫
read_ev41_twice() {
  (( $(grep -cE 'Z read([[:space:]]|$)' "$D41/tasks/$T41DONE/events.log" 2>/dev/null) >= 2 ))
}

tui41_run q
# 41a 四面板都畫得出來
assert "41a 四面板：WORKERS 欄可見" wait_for 10 ui41_shows "WORKERS"
assert "41a 四面板：TASKS 欄可見" ui41_shows "TASKS"
assert "41a 四面板：DETAIL 欄可見" ui41_shows "DETAIL"
assert "41a 四面板：ORIGINS 欄可見（P4.6 題 2 改名）" ui41_shows "ORIGINS"
# TASKS 欄含終態（in-flight 之外的那一筆），且顯示權威狀態字
assert "41a TASKS 欄含終態任務（$T41DONE completed）" ui41_shows "$T41DONE"
assert "41a TASKS 欄 status 為權威字 completed" ui41_shows "completed"
assert "41a TASKS 欄亦含 queued 任務" ui41_shows "$T41Q"
# DETAIL 隨聚焦面板的選中項（起始＝WORKERS 的 worker 列）
assert "41a DETAIL 顯示選中 worker 的欄位" ui41_shows "name   : tuiw41"
assert "41a DETAIL evidence 區列唯讀等價 CLI 原文" ui41_shows "\$ agent-bridge list --long"
assert "41a footer 補上 r／i／c 鍵位提示（英文 chrome）" ui41_shows "c copy evidence"
# 題 3：worker DETAIL 把 origin window 與 agent 自己的死活拆成兩列
assert "41a DETAIL 有 origin 列（window 名稱＋@id＋三態）" \
  ui41_shows "origin : $SESS41:$WIN41NAME ("
assert "41a DETAIL 的 agent 死活是另一列" ui41_shows "state  : live"

# 41b `i`：worker 摘要頁（registry 額外欄位＋三態 liveness）
tmx send-keys -t "$TUI41" i
assert "41b i 摘要頁顯示 spawn_tag（世代）" wait_for 10 ui41_shows "spawn_tag    : t41-gen1"
assert "41b i 摘要頁顯示 registered_at" ui41_shows "registered_at: 2026-07-31T04:41:00Z"
assert "41b i 摘要頁顯示 liveness（三態，此處 live）" ui41_shows "liveness     : live"
assert "41b i 摘要頁的 evidence 區唯讀" ui41_shows "agent-bridge list --long"
tmx send-keys -t "$TUI41" Escape
assert "41b 任意鍵關閉摘要頁（摘要內容從畫面消失）" wait_for 10 ui41_lacks "spawn_tag"
assert "41b 關閉後仍在 dashboard" ui41_shows "DETAIL"

# 41c Tab 三欄循環：ORIGINS → WORKERS → TASKS → ORIGINS（DETAIL 不可聚焦）
assert "41c 起始聚焦 WORKERS（選中 marker 落在 worker 列）" \
  ui41_matches "▶ ▸ tuiw41"
tmx send-keys -t "$TUI41" Tab
assert "41c Tab → TASKS（選中 marker 落在終態任務列）" \
  wait_for 10 ui41_matches "▶ . $T41DONE"
assert "41c TASKS 聚焦後 DETAIL 換成該 task 的細節" ui41_shows "task-id: $T41DONE"
assert "41c DETAIL evidence 區為唯讀 read 命令原文" ui41_shows "\$ agent-bridge read $T41DONE"

# 41d gate (a)：`c` 複製證據 → tmux buffer 讀回
tmx send-keys -t "$TUI41" c
assert "41d c 的結果回到 footer" \
  wait_for 10 ui41_shows "evidence copied to the tmux buffer"
tmx show-buffer > "$TESTROOT/t41-buffer.txt" 2>/dev/null
assert "41d gate (a)：buffer 含 immutable task id" \
  grep -qF "task-id: $T41DONE" "$TESTROOT/t41-buffer.txt"
assert "41d gate (a)：buffer 含 agent-bridge read <id>" \
  grep -qF "agent-bridge read $T41DONE" "$TESTROOT/t41-buffer.txt"
assert "41d gate (a)：buffer 含 agent-bridge status <id>" \
  grep -qF "agent-bridge status $T41DONE" "$TESTROOT/t41-buffer.txt"
assert "41d gate (a)：buffer 含 pane id（介入用證據）" \
  grep -qF "pane: $P41A" "$TESTROOT/t41-buffer.txt"
for _m41 in cancel evict despawn send spawn relay unregister register kill gc; do
  assert_fails "41d gate (a)：buffer MUST NOT 含 mutation 子指令 '$_m41'" \
    grep -qF "$_m41" "$TESTROOT/t41-buffer.txt"
done

# 41e 終態任務按 x：提示無效、不開確認框、狀態不動
tmx send-keys -t "$TUI41" x
assert "41e 終態列按 x 被拒（提示含 terminal）" \
  wait_for 10 ui41_shows "is already terminal"
assert "41e 終態列按 x 不開確認框（無等價 CLI 原文）" \
  bash -c '! grep -qF "Confirm to run the equivalent CLI" "$1"' _ "$TESTROOT/tui41-cap.txt"
assert "41e 終態任務狀態未被動過" st_is "$D41" "$T41DONE" completed

# 41f gate (b) 的畫面側：`r` 讀全文
tmx send-keys -t "$TUI41" r
assert "41f gate (b)：r 的 overlay 顯示 response 特徵行" \
  wait_for 10 ui41_shows "AB41-RESPONSE-LINE"
assert "41f gate (b)：overlay 標頭三欄同 CLI stderr（task-id/from/to）" \
  ui41_shows "task-id: $T41DONE"
assert "41f gate (b)：特徵行與 CLI stdout 同一份內容" \
  grep -qF "AB41-RESPONSE-LINE" "$TESTROOT/t41-cli-read.out"
# `r` 走的是與 CLI 同一份實作（同樣記 read 事件，CLI-READ-1）。門檻必須是
# **第 2 筆**：同一 fixture 前面已由 CLI read 記過一筆，只驗「至少一筆」的話
# TUI 完全沒記事件也會綠（跨廠審查 minor #4）
assert "41f r 記 read 事件（與 CLI read 同一份實作）" \
  wait_for 10 read_ev41_twice
tmx send-keys -t "$TUI41" Escape
assert "41f Esc 關閉 overlay（回到 dashboard）" wait_for 10 ui41_shows "DETAIL"

# 41g 未回覆的任務按 r：逐字沿用 core 的拒絕訊息，不開 overlay
tmx send-keys -t "$TUI41" j
tmx send-keys -t "$TUI41" r
assert "41g queued 任務按 r：footer 顯示 core 的拒絕訊息" \
  wait_for 10 ui41_shows "尚未回覆"
assert "41g 被拒時不開 overlay（dashboard 仍在）" ui41_shows "DETAIL"

# 41h Tab 循環走完一圈：TASKS → ORIGINS → WORKERS
tmx send-keys -t "$TUI41" Tab
# P4.6 題 2：origin 列不再有 ●／✗ glyph，marker 之後直接是誠實標籤
assert "41h Tab → ORIGINS（marker 落在 origin 列，無 liveness glyph）" \
  wait_for 10 ui41_matches "▶ ▸ $SESS41:$WIN41NAME"
tmx send-keys -t "$TUI41" Tab
assert "41h Tab → WORKERS（循環回到起點）" wait_for 10 ui41_matches "▶ ▸ tuiw41"

# 41j Enter matrix（P4.6 切片 B）：**WORKERS 的內嵌 task 列 Enter＝read**，
# 不再 focus 所屬 worker。WORKERS 欄只列 in-flight，這裡唯一的內嵌任務是
# queued 的 $T41Q——讀不動，於是同一條斷言同時證明兩件事：走的是 read 那條
# 路，且拒絕由 core 權威回答（不造旁路）。
#
# 先按 x 在 footer 種一個哨兵訊息：41g 早就讓「尚未回覆」留在 footer 上了，
# 不換掉的話 Enter 什麼都不做也會綠
tmx send-keys -t "$TUI41" x
assert "41j 前置：footer 上是哨兵訊息（x 對 worker 列無效）" \
  wait_for 10 ui41_shows "x only acts on task rows"
tmx send-keys -t "$TUI41" j
tmx send-keys -t "$TUI41" Enter
assert "41j WORKERS task 列 Enter＝read（core 的拒絕訊息逐字進 footer）" \
  wait_for 10 ui41_shows "尚未回覆"
assert "41j Enter 確實動了（哨兵訊息已被換掉）" ui41_lacks "x only acts on task rows"
assert "41j task 列的 Enter MUST NOT focus 它的 worker" ui41_lacks "focused 'tuiw41'"
assert "41j 被拒時不開 overlay（dashboard 仍在）" ui41_shows "DETAIL"

# 41k ORIGINS 列 Enter：焦點切 WORKERS 並選該 scope 的第一個 worker
# （此刻選取還停在上面那個 task 列，所以 `▶ ▸ tuiw41` 是真的被移回來的）
tmx send-keys -t "$TUI41" BTab
assert "41k 前置：BTab 回到 ORIGINS" \
  wait_for 10 ui41_matches "▶ ▸ $SESS41:$WIN41NAME"
tmx send-keys -t "$TUI41" Enter
assert "41k ORIGINS Enter → 焦點切 WORKERS 且選中第一個 worker" \
  wait_for 10 ui41_matches "▶ ▸ tuiw41"

# 41l contextual footer（P4.6 切片 B）：第一段只列當前列有效的鍵、第二段全域
assert "41l footer 第一段隨選中列（worker 列＝focus pane）" ui41_shows "Enter focus pane"
assert "41l worker 列上 MUST NOT 列出 x（它只對 task 列有效）" ui41_lacks "x cancel"
assert "41l footer 第二段是全域鍵" ui41_shows "? keys"

# 41i 退出：termios 逐字還原、tmux 世界不變
tmx send-keys -t "$TUI41" q
assert "41i q 退出：離開 alternate screen（主畫面哨兵行回來）" \
  wait_for 10 ui41_shows "AB41-MAIN-SCREEN"
assert "41i q 退出：termios 逐字還原" wait_for 10 stty41_same q
tmx list-windows -F '#{window_id} #{window_layout}' > "$TESTROOT/tui41-after.txt"
assert "41i 退出後 window id／layout／geometry 不變" \
  diff -q "$TESTROOT/tui41-before.txt" "$TESTROOT/tui41-after.txt"

tmx kill-pane -t "$TUI41" 2>/dev/null || true
tmx kill-window -t "$W41_WIN" 2>/dev/null || true

else
  printf 'SKIP: 41 TUI 四面板＋唯讀鍵（SRC_KIND=bash：bash 正本凍結在 M4，不含 TUI）\n'
fi

# ---- 42. evict 入口 CAS（CLI-EVICT-4） ----
# spec: CLI-EVICT-4 CLI-EVICT-3
# 設計正本 docs/tui-design.md §5／§9 P3 的三則 gate：
#   (a) expect 相符 → evict 成功
#   (b) **invocation 前已換代** → selection stale 非 0 退出，且不建 task、
#       無通知、pane 未 kill、registry 未動、審計未新增
#   (c) 送出後 → despawn 前換代 → 照舊拒收（既有 CLI-EVICT-3 行為，
#       由分組 26 覆蓋；本組只確認新增的入口鎖沒有削弱它）
# 與 35–41 同理只對 Rust 執行（bash 正本自 M4 凍結，不含 --expect-*）。
if [[ "$SRC_KIND" == rust ]]; then

D42="$TESTROOT/d42"

# agents.log 的行數（證明「被拒時審計未新增」）
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
audit_lines42() { wc -l < "$D42/agents.log" 2>/dev/null || printf '0'; }

pane42="$(absp "$D42" 0 spawn ev42 --runtime codex 2>/dev/null)"
GEN42="$(jq -r '.spawn_tag' "$D42/agents/ev42.json")"
assert "42 前置：spawn_tag 讀得到（世代綁定的前提）" test -n "$GEN42"

# 42a 參數面（additive）：值不得為空、缺參數被拒；被拒時不留孤兒任務
before42="$(task_count "$D42")"
before_audit42="$(audit_lines42)"
assert_fails "42a --expect-pane 缺參數被拒" ab "$D42" evict ev42 --expect-pane
assert_fails "42a --expect-generation 缺參數被拒" ab "$D42" evict ev42 --expect-generation
assert_fails "42a --expect-pane 空值被拒" ab "$D42" evict ev42 --expect-pane ""
assert "42a 參數被拒時不留孤兒收尾任務" test "$(task_count "$D42")" -eq "$before42"

# 42b gate (b)：**invocation 前已換代**。以舊值呼叫 → selection stale，
# 且此刻 MUST 一個副作用都沒有
cp "$D42/agents/ev42.json" "$TESTROOT/ev42-before.json"
jq --arg t "$GEN42-NEXT" '.spawn_tag = $t' "$TESTROOT/ev42-before.json" \
  > "$D42/agents/ev42.json"
cp "$D42/agents/ev42.json" "$TESTROOT/ev42-snapshot.json"
before42="$(task_count "$D42")"
before_audit42="$(audit_lines42)"
ab "$D42" evict ev42 --expect-generation "$GEN42" \
  > "$TESTROOT/ev42-stale.out" 2> "$TESTROOT/ev42-stale.err" && rc42=0 || rc42=$?
assert "42b 世代不符：非 0 退出" test "$rc42" -ne 0
assert "42b 世代不符：訊息含 selection stale" \
  grep -qF "selection stale" "$TESTROOT/ev42-stale.err"
assert "42b 世代不符：**不建 task**（tasks/ 數量不變）" \
  test "$(task_count "$D42")" -eq "$before42"
assert "42b 世代不符：pane 未被 kill" pane_alive "$pane42"
assert "42b 世代不符：registry 未被動（逐字元不變）" \
  diff -q "$TESTROOT/ev42-snapshot.json" "$D42/agents/ev42.json"
assert "42b 世代不符：審計未新增" test "$(audit_lines42)" -eq "$before_audit42"
assert "42b 世代不符：stdout 空（沒有 task-id 可印）" test ! -s "$TESTROOT/ev42-stale.out"

# 42c pane 不符走同一條防線（只帶 --expect-pane）
before42="$(task_count "$D42")"
ab "$D42" evict ev42 --expect-pane "%999999" \
  > /dev/null 2> "$TESTROOT/ev42-pane.err" && rc42=0 || rc42=$?
assert "42c pane 不符：非 0 退出" test "$rc42" -ne 0
assert "42c pane 不符：訊息含 selection stale" \
  grep -qF "selection stale" "$TESTROOT/ev42-pane.err"
assert "42c pane 不符：不建 task" test "$(task_count "$D42")" -eq "$before42"
assert "42c pane 不符：pane 未被 kill" pane_alive "$pane42"

# 42f 起始狀態 MUST 是**有效**的：42b 留下的 stale registry 若不還原，evict
# 一進門就被擋，那又退回成「呼叫前就 stale」的形狀，殺不到 mutant
cp "$TESTROOT/ev42-before.json" "$D42/agents/ev42.json"

# 42f 鎖邊界（本組唯一能殺 mutant 的一條）：42b／42c 只證明「stale 會被擋」，
# 但那在「先比對、後取鎖」的錯誤實作下**照樣綠**——registry 在呼叫前就已經
# 是 stale 了。這裡改成讓換代發生在 evict **已經開始、正卡在鎖上**的時候：
#   佔住 agents-registry 鎖 → 背景啟動帶舊 expect 的 evict（卡住）
#   → 換代 → 放鎖 → evict 取得鎖後 MUST 重讀並判 stale
# 「先比對後取鎖」的實作會在卡住之前就讀到舊值、比對通過，於是建出 task。
mkdir -p "$D42/locks"
LOCK42="$D42/locks/agents-registry.lock"
mkdir "$LOCK42"
before42="$(task_count "$D42")"
# --timeout 1：正確實作會在 await 之前就 stale 掉，根本走不到那裡；帶上它是
# 為了讓**錯誤實作**快速失敗而不是掛在預設 300s 的 await 上（實測 mutant 會）
( ab "$D42" evict ev42 --expect-generation "$GEN42" --timeout 1 \
    > "$TESTROOT/ev42-race.out" 2> "$TESTROOT/ev42-race.err"; \
  printf '%s' "$?" > "$TESTROOT/ev42-race.rc" ) &
race42_pid=$!
sleep 0.6
# evict 此刻 MUST 還沒建任何 task（它卡在鎖上，還沒讀 registry）
assert "42f evict 卡在鎖上時尚未建任何 task" \
  test "$(task_count "$D42")" -eq "$before42"
jq --arg t "$GEN42-RACE" '.spawn_tag = $t' "$TESTROOT/ev42-before.json" \
  > "$D42/agents/ev42.json"
rmdir "$LOCK42"
wait "$race42_pid" 2>/dev/null || true
assert "42f 取得鎖後重讀 registry：判 selection stale" \
  grep -qF "selection stale" "$TESTROOT/ev42-race.err"
assert "42f 鎖競爭下非 0 退出" test "$(<"$TESTROOT/ev42-race.rc")" -ne 0
assert "42f 鎖競爭下不建 task（比對若在取鎖前做，這裡會多一個）" \
  test "$(task_count "$D42")" -eq "$before42"

# 42g 「不通知」是 spec 明文（CLI-EVICT-4），pane_alive 證不到它——用計數
# shim 直接數：selection stale 路徑 MUST 一次 send-keys 都沒有
SHIM42="$TESTROOT/shim42"
mkdir -p "$SHIM42"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >> %q\nexec %q "$@"\n' \
  "$TESTROOT/tmux-calls42.txt" "$(command -v tmux)" > "$SHIM42/tmux"
chmod +x "$SHIM42/tmux"
: > "$TESTROOT/tmux-calls42.txt"
before42="$(task_count "$D42")"
PATH="$SHIM42:$PATH" ab "$D42" evict ev42 --expect-generation "$GEN42" \
  > /dev/null 2>&1 || true
assert "42g selection stale：一次 send-keys 都沒有（＝沒通知任何人）" \
  bash -c '! grep -q "send-keys" "$1"' _ "$TESTROOT/tmux-calls42.txt"
assert "42g selection stale：仍未建 task" test "$(task_count "$D42")" -eq "$before42"

# 換代模擬到此為止：registry 的 tag 已與 pane 的 pane_start_command 分家，
# 留著會讓 42d 的 despawn 走 stale 路徑（那是 CLI-EVICT-3 的既有防線，不是
# 本組要測的東西）。還原成真值，42d 才驗得到「相符 → 真的回收」
cp "$TESTROOT/ev42-before.json" "$D42/agents/ev42.json"

# 42d gate (a)：expect 相符（兩項都帶且都對）→ 走完整流程並成功回收
bg_reply "$D42" ev42 "收尾筆記：CAS 相符路徑"
tid42="$(ab "$D42" evict ev42 --from alice \
  --expect-pane "$pane42" --expect-generation "$GEN42" \
  2> "$TESTROOT/ev42-ok.err")" && rc42=0 || rc42=$?
assert "42d expect 相符：exit 0" test "$rc42" -eq 0
assert "42d expect 相符：stdout 是真的收尾 task-id" test -d "$D42/tasks/$tid42"
assert "42d expect 相符：收尾任務派給被驅逐者" \
  test "$(jq -r '.to' "$D42/tasks/$tid42/metadata.json")" = ev42
assert "42d expect 相符：registry 已回收" test ! -f "$D42/agents/ev42.json"
assert "42d expect 相符：審計記了 evicted*" \
  grep -qE 'evicted' "$D42/agents.log"

# 42e 不帶 expect 參數＝行為與現行完全相同（additive 的核心承諾）
pane42b="$(absp "$D42" 0 spawn ev42b --runtime codex 2>/dev/null)"
bg_reply "$D42" ev42b "收尾筆記：不帶 expect 的既有路徑"
tid42b="$(ab "$D42" evict ev42b --from alice 2> "$TESTROOT/ev42b.err")" \
  && rc42=0 || rc42=$?
assert "42e 不帶 expect：exit 0（既有行為不變）" test "$rc42" -eq 0
assert "42e 不帶 expect：收尾任務落地" test -d "$D42/tasks/$tid42b"
assert "42e 不帶 expect：registry 已回收" test ! -f "$D42/agents/ev42b.json"
tmx kill-pane -t "$pane42b" 2>/dev/null || true

else
  printf 'SKIP: 42 evict 入口 CAS（SRC_KIND=bash：bash 正本凍結在 M4，不含 --expect-*）\n'
fi

# ---- 43. TUI evict 證據框（CAS） ----
# spec: CLI-UI-1 CLI-EVICT-4 CLI-EVICT-3
# 設計正本 docs/tui-design.md §3／§5 的 `e`：
#   (a) worker 列按 e → 證據框措辭是「派收尾任務後回收」（P4.6 題 9 英文化後
#       即 `wrap-up task, then reclaim`）、逐字顯示等價 CLI 原文（含兩個
#       --expect-*），且畫面上不得出現任何「安全刪除」語彙（中英兩套都擋）
#   (b) n／Esc 放棄 → 一個副作用都沒有
#   (c) 開框後換代 → y 確認時**重讀 registry**判 selection stale：不建 task、
#       pane 未 kill（TUI 側的 compare-and-act，與分組 42 的 CLI 側成對）
#   (d) 相符 → 走下沉的 core 編排：收尾任務落地後 registry 回收、pane 被 kill
#   (e) evict 的 await 段（預設 300s）跑在**一次性 thread** 上：期間 UI 仍收得
#       了鍵（? 開得了合法鍵頁），同一個 worker 再按 e 只提示「進行中」
# 與 40–42 同理只對 Rust 執行（bash 正本自 M4 凍結，不含 TUI）。
if [[ "$SRC_KIND" == rust ]]; then

D43="$TESTROOT/d43"
mkdir -p "$D43"

# 真 spawn：pane 的 start command 帶 spawn_tag，despawn 才認得出這一代
# （手寫 registry 會讓 tag 與 pane 分家，evict 一律走 stale 路徑）
# --window：本組跑在 40–42 之後，owner window 已被前面的分組塞滿 pane，
# 共用視窗的 split-window 會失敗（實測）。專屬 window 讓 fixture 與前面的
# 分組脫鉤
pane43="$(absp "$D43" 0 spawn ev43 --runtime codex --window 2>/dev/null)"
pane43b="$(absp "$D43" 0 spawn ev43b --runtime codex --window 2>/dev/null)"
GEN43="$(jq -r '.spawn_tag' "$D43/agents/ev43.json")"
GEN43B="$(jq -r '.spawn_tag' "$D43/agents/ev43b.json")"
assert "43 前置：兩個 worker 都 spawn 成功" \
  test -n "$pane43" -a -n "$pane43b" -a -f "$D43/agents/ev43b.json"
assert "43 前置：spawn_tag 讀得到（證據框要顯示它）" test -n "$GEN43"

TUI43="$(tmx new-window -dP -F '#{pane_id}' -t it "$pane_cmd")"
OWN43="$(tmx display -p -t "$TUI43" '#{session_name}:#{window_id}')"
# spawn 記的 owner 是測試腳本所在的位置，不是 TUI 的；改掛到 TUI 所在 owner，
# 首頁（以 current owner 為根）才看得到這兩個 worker。其餘欄位一律不動
for _w43 in ev43 ev43b; do
  jq --arg o "$OWN43" '.owner = $o' "$D43/agents/$_w43.json" \
    > "$TESTROOT/$_w43.owner.json"
  mv "$TESTROOT/$_w43.owner.json" "$D43/agents/$_w43.json"
done
cp "$D43/agents/ev43.json" "$TESTROOT/ev43-before.json"

tmx send-keys -t "$TUI43" \
  "$(printf 'echo AB43-MAIN-SCREEN; touch %q' "$TESTROOT/tui43-ready")" Enter
wait_for 10 test -f "$TESTROOT/tui43-ready"

cap43() { tmx capture-pane -p -t "$TUI43" > "$TESTROOT/tui43-cap.txt" 2>/dev/null; }
# shellcheck disable=SC2329  # 經 assert/wait_for 的 "$@" 間接呼叫
ui43_shows() { cap43 && grep -qF -- "$1" "$TESTROOT/tui43-cap.txt"; }
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
ui43_lacks() { cap43 && ! grep -qF -- "$1" "$TESTROOT/tui43-cap.txt"; }
# shellcheck disable=SC2329  # 經 wait_for 間接呼叫
pane_gone43() { ! pane_alive "$1"; }

tmx send-keys -t "$TUI43" \
  "$(printf 'env AGENT_BRIDGE_DATA=%q %q ui' "$D43" "$BRIDGE")" Enter
assert "43 UI 起得來（WORKERS 欄看得到 ev43）" wait_for 10 ui43_shows "ev43"

# 43a 證據框：措辭＋等價 CLI 原文（§2 薄殼原則／§5 顯示紀律）
before43="$(task_count "$D43")"
tmx send-keys -t "$TUI43" e
assert "43a e 開證據框：措辭是「派收尾任務後回收」（英文 chrome）" \
  wait_for 10 ui43_shows "wrap-up task, then reclaim"
assert "43a 證據框逐字顯示等價 CLI（含兩個 --expect-*）" \
  ui43_shows "agent-bridge evict ev43 --expect-pane $pane43 --expect-generation $GEN43"
assert "43a 證據框 MUST NOT 出現「安全刪除」語彙" ui43_lacks "安全刪除"
assert "43a 證據框 MUST NOT 出現英文的「安全刪除」語彙" ui43_lacks "safe to delete"
assert "43a 開框本身沒有副作用（未建 task）" \
  test "$(task_count "$D43")" -eq "$before43"

# 43b n 放棄：不建 task、registry 不動
tmx send-keys -t "$TUI43" n
assert "43b n 放棄 evict（footer 明說）" wait_for 10 ui43_shows "evict aborted"
assert "43b 放棄後未建任何 task" test "$(task_count "$D43")" -eq "$before43"
assert "43b 放棄後 pane 仍在" pane_alive "$pane43"

# 43c gate (c)：開框之後換代 → y 確認時重讀 registry → selection stale。
# 「沿用輪詢快照」的實作在這裡會照樣派出收尾任務（多一個 task）
tmx send-keys -t "$TUI43" e
assert "43c 前置：證據框已開（顯示當時世代 $GEN43）" \
  wait_for 10 ui43_shows "--expect-generation $GEN43"
jq --arg t "$GEN43-NEXT" '.spawn_tag = $t' "$TESTROOT/ev43-before.json" \
  > "$TESTROOT/ev43-next.json"
mv "$TESTROOT/ev43-next.json" "$D43/agents/ev43.json"
before43="$(task_count "$D43")"
# 「無副作用」要驗得到位（codex 複核 minor #6）：換代之後、按 y 之前先取
# registry 與審計的基準——之後的 diff 才問得到「evict 有沒有動過它」
cp "$D43/agents/ev43.json" "$TESTROOT/ev43-stale-snapshot.json"
audit_before43="$(wc -l < "$D43/agents.log" 2>/dev/null || printf '0')"
tmx send-keys -t "$TUI43" y
assert "43c 確認當下重讀 registry：判 selection stale" \
  wait_for 10 ui43_shows "selection stale"
assert "43c selection stale：不建 task" test "$(task_count "$D43")" -eq "$before43"
assert "43c selection stale：pane 未被 kill" pane_alive "$pane43"
assert "43c selection stale：registry 逐 byte 未被動" \
  diff -q "$TESTROOT/ev43-stale-snapshot.json" "$D43/agents/ev43.json"
assert "43c selection stale：審計未新增" \
  test "$(wc -l < "$D43/agents.log" 2>/dev/null || printf '0')" -eq "$audit_before43"
# 換代模擬到此為止：tag 與 pane 的 start command 已分家，留著會讓 43d 的
# despawn 走 stale 路徑（那是 CLI-EVICT-3 的既有防線，不是本組要測的東西）
cp "$TESTROOT/ev43-before.json" "$D43/agents/ev43.json"

# 43d gate (d)：相符 → 走下沉到 ab-core 的完整編排（send → await → despawn）
bg_reply "$D43" ev43 "收尾筆記：TUI 證據框路徑"
tmx send-keys -t "$TUI43" e
assert "43d 前置：證據框重開（世代已還原）" \
  wait_for 10 ui43_shows "--expect-generation $GEN43"
tmx send-keys -t "$TUI43" y
assert "43d 確認後 registry 被回收（evict 完成）" \
  wait_for 30 test ! -f "$D43/agents/ev43.json"
assert "43d 確認後 pane 已被 kill" wait_for 10 pane_gone43 "$pane43"
assert "43d 審計記了 evicted*" grep -qE 'evicted' "$D43/agents.log"
assert "43d 終局回到 footer（背景一次性 thread → channel）" \
  wait_for 10 ui43_shows "evicted 'ev43'"

# 43e gate (e)：evict 的 await 段跑在一次性 thread 上（常駐 worker 那條同時
# 負責 liveness 輪詢，搭上去等於整整五分鐘不再刷新）。ev43b 沒有人回覆，
# 編排會停在 await；此時 UI MUST 仍收得了鍵
tmx send-keys -t "$TUI43" e
assert "43e ev43b 的證據框開得起來" wait_for 10 ui43_shows "agent-bridge evict ev43b"
tmx send-keys -t "$TUI43" y
assert "43e 收尾任務已派出、進入等待（footer 看得到）" \
  wait_for 15 ui43_shows "等待筆記落地"
tmx send-keys -t "$TUI43" '?'
assert "43e await 期間 UI 仍收得了鍵（? 開得了合法鍵頁）" \
  wait_for 10 ui43_shows "keys (current selection)"
tmx send-keys -t "$TUI43" k   # 任意鍵關閉合法鍵頁
tmx send-keys -t "$TUI43" e
assert "43e in-flight 閘：同一個 worker 再按 e 只提示進行中" \
  wait_for 10 ui43_shows "already in progress"

tmx send-keys -t "$TUI43" q
assert "43e q 退出（UI thread 沒被 evict 的 await 卡住）" \
  wait_for 10 ui43_shows "AB43-MAIN-SCREEN"

# 43f 窄畫面（100 欄）：evict 的等價 CLI 原文約 112 字元，截斷會把尾端的
# generation 吃掉——而那正是人判斷「要不要按 y」的依據（codex 複核 minor #5）。
# 200 欄的 TUI43 抓不到這條，所以另開一個 manual-size 的窄 window 再驗一次。
TUI43N="$(tmx new-window -dP -F '#{window_id}' -t it "$pane_cmd")"
tmx set-option -w -t "$TUI43N" window-size manual
tmx resize-window -t "$TUI43N" -x 100 -y 40
P43N="$(tmx list-panes -t "$TUI43N" -F '#{pane_id}' | head -1)"
# shellcheck disable=SC2329  # 經 assert/wait_for 的 "$@" 間接呼叫
ui43n_shows() {
  tmx capture-pane -p -t "$P43N" > "$TESTROOT/tui43n-cap.txt" 2>/dev/null \
    && grep -qF -- "$1" "$TESTROOT/tui43n-cap.txt"
}
# 證據框裡的 generation 出現在**兩處**：短的「世代 :」欄，以及長的等價 CLI
# 原文那一行。截斷時只有前者活得下來——所以「≥2 行命中」才是「命令原文沒被
# 截掉尾巴」的判準（只驗 ≥1 的話，把 wrap 拿掉照樣綠，實測過）
# shellcheck disable=SC2329  # 經 assert/wait_for 的 "$@" 間接呼叫
ui43n_tag_twice() {
  tmx capture-pane -p -t "$P43N" > "$TESTROOT/tui43n-cap.txt" 2>/dev/null \
    && [[ "$(grep -c -F -- "$1" "$TESTROOT/tui43n-cap.txt")" -ge 2 ]]
}
assert "43f 前置：窄 window 真的是 100 欄" \
  bash -c '[[ "$1" == 100 ]]' _ "$(tmx display -p -t "$TUI43N" '#{window_width}')"
tmx send-keys -t "$P43N" \
  "$(printf 'env AGENT_BRIDGE_DATA=%q %q ui' "$D43" "$BRIDGE")" Enter
assert "43f 窄畫面下 UI 起得來" wait_for 10 ui43n_shows "ev43b"
tmx send-keys -t "$P43N" e
assert "43f 窄畫面下證據框措辭仍在" \
  wait_for 10 ui43n_shows "wrap-up task, then reclaim"
assert "43f 窄畫面下 generation 欄可見" ui43n_shows "$GEN43B"
assert "43f 窄畫面下等價 CLI 的 generation 未被截掉（換行保留整條命令）" \
  wait_for 10 ui43n_tag_twice "$GEN43B"
tmx send-keys -t "$P43N" n
tmx send-keys -t "$P43N" q
tmx kill-window -t "$TUI43N" 2>/dev/null || true

tmx kill-pane -t "$TUI43" 2>/dev/null || true
tmx kill-pane -t "$pane43b" 2>/dev/null || true

else
  printf 'SKIP: 43 TUI evict 證據框（SRC_KIND=bash：bash 正本凍結在 M4，不含 TUI）\n'
fi

# ---- 44. P4 效率驗收（replay script 步數 gate） ----
# spec: CLI-UI-1 CLI-LIST-2
# 設計正本 docs/tui-design.md §9 P4：固定 fixture（3 owner／12 worker／
# 20 task／3 異常）＋兩份固定 replay script（baseline：list --long＋合法唯讀
# 命令序列；TUI：key 序列）＋明確成功 marker（三個異常 id 均被輸出／複製）。
# Gate：TUI 步數 ≤ baseline 的 50%，正確率 100%。
#
# 計數規則＝script 內的按鍵／命令步數，**從 script 檔機械導出**
# （tests/replay/p4-baseline.cmds／p4-tui.keys，非註解非空行＝一步）——
# 手寫死數字會在 script 改動時悄悄失準。
#
# 附註（設計原文）：replay script 是 fixture 的一部分（固定初始 selection 與
# 異常排序位置），量的是「固定操作序列下的步數差」，不宣稱量到任意操作者的
# 自由行為。
#
# 前置：BLOCKER 軸（§4 v1 matcher 契約：硬編碼 prompt matcher ＋結構性
# occlusion）——沒有它 TUI 定位不到 blocked prompt，正確率必然 2/3。
# 與 40–43 同理只對 Rust 執行。
if [[ "$SRC_KIND" == rust ]]; then

D44="$TESTROOT/d44"
P4OUT="$TESTROOT/p4out"
mkdir -p "$D44/agents" "$D44/tasks" "$P4OUT"
# baseline script 用的 CLI 載具（唯讀命令；PATH 走 shim 讓 tmux 指到測試 socket）
# shellcheck disable=SC2329  # 經 replay script 的 eval 間接呼叫
ab_p4() { env AGENT_BRIDGE_DATA="$D44" PATH="$SHIM:$PATH" "$BRIDGE" "$@"; }

# owner A＝TUI 自己所在的 window（§2「以 current owner 為根」→ 首頁 selection
# 固定落在它）。**TUI 獨佔這個 window**：worker 的 pane 另開一個 window 擺
# ——owner 記的是「誰派出這個 worker」，pane 在哪是另一回事（`list --long` 的
# OWNER 與 WHERE 本來就是兩欄）。擠在同一個 window 會把 TUI 壓成十來行高，
# 量到的就變成捲動成本而不是定位成本。
W44A="$(tmx new-window -dP -F '#{window_id}' -t it "$pane_cmd")"
TUI44="$(tmx list-panes -t "$W44A" -F '#{pane_id}' | head -1)"
OWN44A="$(tmx display -p -t "$TUI44" '#{session_name}:#{window_id}')"
# P4.6 題 1：ORIGINS 欄顯示的是 window **名稱**。owner A／B 的 window 都活著，
# 標籤裡的假 session 名（`zzb:`）在顯示時會被**當下查到的**真 session 名取代
# ——所以兩者要靠 window 名字才分得開（否則畫面上都是 `it:<預設名>`）
W44ANAME=p4a
tmx rename-window -t "$W44A" "$W44ANAME"
# owner A 的 6 個 worker pane（p4w01..p4w06）
W44P="$(tmx new-window -dP -F '#{window_id}' -t it "$pane_cmd")"
P4P01="$(tmx list-panes -t "$W44P" -F '#{pane_id}' | head -1)"
for _i in 2 3 4 5 6; do
  eval "P4P0$_i=\"\$(tmx split-window -dP -F '#{pane_id}' -t \"\$W44P\" \"\$pane_cmd\")\""
done
# split-window 每次對半切 active pane：不重排的話最後一個 pane 只剩 2-3 行，
# capture-pane 抓不到完整的權限框畫面（實測踩過）
tmx select-layout -t "$W44P" even-vertical

# owner B／C 的標籤用**受控的 session 名**＋真實 window id：owner 欄的死活
# 只看 `@winid`（`owner_liveness`／`list --long` 都是），而 session 名決定
# ORIGINS 欄的字典序（**排序看標籤不看顯示字面**）。`it:@N` < `zzb:…` <
# `zzc:…`，三個 origin 的排序因此固定
# ——那是 replay script 的一部分（設計 §9 P4 附註：固定異常排序位置）。
W44B="$(tmx new-window -dP -F '#{window_id}' -t it "$pane_cmd")"
W44BNAME=p4b
tmx rename-window -t "$W44B" "$W44BNAME"
P4P07="$(tmx list-panes -t "$W44B" -F '#{pane_id}' | head -1)"
P4P08="$(tmx split-window -dP -F '#{pane_id}' -t "$W44B" "$pane_cmd")"
P4P10="$(tmx split-window -dP -F '#{pane_id}' -t "$W44B" "$pane_cmd")"
P4P11="$(tmx split-window -dP -F '#{pane_id}' -t "$W44B" "$pane_cmd")"
tmx select-layout -t "$W44B" even-vertical
OWN44B="zzb:$W44B"
# owner C：建完就 kill——它的 window id 於是不存在＝dead owner 異常
W44C="$(tmx new-window -dP -F '#{window_id}' -t it "$pane_cmd")"
tmx kill-window -t "$W44C" 2>/dev/null || true
OWN44C="zzc:$W44C"

# registry：12 個 worker／3 個 owner。名字一律 p4wNN（**中性命名**：名字不
# 洩漏誰是異常，異常的字典序位置因此不是為了好看而挑的）
w44() { # w44 <name> <pane> <owner>
  printf '{"name":"%s","pane_id":"%s","registered_at":"2026-07-31T00:00:00Z","spawned":true,"ready":true,"runtime":"codex","spawn_tag":"t44-%s","owner":"%s"}\n' \
    "$1" "$2" "$1" "$3" > "$D44/agents/$1.json"
}
w44 p4w01 "$P4P01" "$OWN44A"
w44 p4w02 "$P4P02" "$OWN44A"
w44 p4w03 "$P4P03" "$OWN44A"
w44 p4w04 "$P4P04" "$OWN44A"
w44 p4w05 "$P4P05" "$OWN44A"
# 異常 2／3：blocked prompt（pane 活著，畫面是權限框）——掃描序中的第 6 個
w44 p4w06 "$P4P06" "$OWN44A"
w44 p4w07 "$P4P07" "$OWN44B"
w44 p4w08 "$P4P08" "$OWN44B"
# 異常 1／3：orphaned worker（registry 有、pane 不在，owner 仍活著）
w44 p4w09 "%94409" "$OWN44B"
w44 p4w10 "$P4P10" "$OWN44B"
w44 p4w11 "$P4P11" "$OWN44B"
# 異常 3／3：dead owner 底下的 worker（owner window 已被 kill）
w44 p4w12 "%94412" "$OWN44C"

# 20 個 task（目錄名後綴必須是 hex，task.rs:378 會拒非 hex）。12 個 in-flight
# ＋8 個終態：WORKERS 欄因此有 task 列（導航噪音），TASKS 欄有東西可讀
_i=0
for _spec in \
  "p4w01 queued" "p4w01 running" "p4w02 delivered" "p4w03 running" \
  "p4w04 queued" "p4w05 running" "p4w06 running" "p4w07 queued" \
  "p4w08 delivered" "p4w09 running" "p4w10 queued" "p4w11 running" \
  "p4w01 completed" "p4w02 completed" "p4w03 failed" "p4w04 completed" \
  "p4w05 cancelled" "p4w06 completed" "p4w07 completed" "p4w12 completed"; do
  set -- $_spec
  _to="$1"; _st="$2"
  _tid="$(printf '20260731T0000%02dZ-44%02x' "$_i" "$((160 + _i))")"
  mkdir -p "$D44/tasks/$_tid"
  printf '%s\n' "$_st" > "$D44/tasks/$_tid/status"
  printf 'p4 fixture task\n' > "$D44/tasks/$_tid/request.md"
  printf 'p4 fixture response\n' > "$D44/tasks/$_tid/response.md"
  printf '{"version":1,"task_id":"%s","from":"alice","to":"%s","created_at":"2026-07-31T00:00:01Z","updated_at":"2026-07-31T00:00:01Z","working_directory":"/tmp","status":"%s"}\n' \
    "$_tid" "$_to" "$_st" > "$D44/tasks/$_tid/metadata.json"
  _i=$((_i + 1))
done
# fixture 的形狀是整組的前提，要驗到**每一個維度**：只數 worker 與 task 的話，
# 「把 12 個 worker 全塞進 2 個 owner」這種 mutation 照樣全綠，而 owner 數正是
# TUI 步數的來源（codex 複核 major #2）。owner 集合逐字比對＋各 owner 的 worker
# 分布，全部從 registry JSON 機械萃取
assert "44 前置：worker 與 task 數（12／20）" \
  bash -c '[[ "$(ls "$1"/agents | wc -l)" == 12 && "$(ls "$1"/tasks | wc -l)" == 20 ]]' _ "$D44"
jq -r '.owner' "$D44"/agents/*.json | sort -u > "$P4OUT/owners.actual"
printf '%s\n' "$OWN44A" "$OWN44B" "$OWN44C" | sort > "$P4OUT/owners.expect"
assert "44 前置：owner 集合恰為 3 個、逐字相符（防兩-owner mutation）" \
  diff -q "$P4OUT/owners.expect" "$P4OUT/owners.actual"
# shellcheck disable=SC2329  # 經 assert 的 "$@" 間接呼叫
p4_owner_count() { jq -r --arg o "$2" 'select(.owner==$o) | .name' "$1"/agents/*.json | wc -l; }
while read -r _own _want; do
  [[ -z "$_own" ]] && continue
  assert "44 前置：owner $_own 底下恰 $_want 個 worker" \
    test "$(p4_owner_count "$D44" "$_own")" -eq "$_want"
done <<EOF
$OWN44A 6
$OWN44B 5
$OWN44C 1
EOF

# blocked prompt 的畫面：用**真的長得像權限框**的內容（notify::screen_has_prompt
# 的第一組錨：`Do you want to …` ＋ `Esc to cancel`），不依賴 matcher 的誤判面
tmx send-keys -t "$P4P06" \
  "clear; printf 'Do you want to make this edit?\\n 1. Yes\\n 2. No, keep going\\n\\nEsc to cancel\\n'" Enter
# 其餘 pane 保持乾淨的 shell 畫面（baseline 掃描要掃得到「不是它」）
# shellcheck disable=SC2329  # 經 wait_for 間接呼叫
p4_blocked_ready() {
  tmx capture-pane -p -t "$P4P06" > "$TESTROOT/p4w06-probe.txt" 2>/dev/null \
    && grep -qF "Esc to cancel" "$TESTROOT/p4w06-probe.txt"
}
assert "44 前置：blocked prompt 的畫面已就緒（異常 2／3 的來源）" \
  wait_for 15 p4_blocked_ready


# ---- 步數：宣告值與執行值必須是**兩個獨立證據** ----
# 只比「行數 vs 行數」是同一個 predicate 自我比較：`true; true; true` 一行三
# 命令會被記成 1 步，步數就被灌水了（codex 複核 major #1，附反例）。兩道防線：
#   (a) **格式收窄**：每一步必須是單一命令——拒絕 `;`／`|`／`&`／命令替換／
#       續行。不合形狀的行直接記成 bad，不執行。
#   (b) **執行計數只在該步成功後才 +1**（baseline 看 rc、TUI 看 send-keys 的
#       rc），與行數導出的宣告值來自完全不同的來源。
p4_steps() { grep -cvE '^[[:space:]]*(#|$)' "$1"; }
# 單一命令的形狀（變數展開允許，命令替換與命令分隔一律拒絕）
p4_cmd_shape_ok() {
  case "$1" in
    *';'* | *'|'* | *'&'* | *'$('* | *'`'* | *'\') return 1 ;;
  esac
  return 0
}
# TUI 按鍵的 allowlist（tmux 鍵名；一行一鍵，不接受任何組合寫法）
p4_key_ok() {
  case "$1" in
    Tab | BTab | Enter | Escape | Up | Down | [a-z] | '?') return 0 ;;
    *) return 1 ;;
  esac
}
BASE_CMDS="$(dirname "${BASH_SOURCE[0]}")/replay/p4-baseline.cmds"
TUI_KEYS="$(dirname "${BASH_SOURCE[0]}")/replay/p4-tui.keys"
STEPS_BASE="$(p4_steps "$BASE_CMDS")"
STEPS_TUI="$(p4_steps "$TUI_KEYS")"

# ---- baseline replay ----
_n=0
_nbad=0
while IFS= read -r _line; do
  [[ "$_line" =~ ^[[:space:]]*(#|$) ]] && continue
  if ! p4_cmd_shape_ok "$_line"; then
    _nbad=$((_nbad + 1))
    continue
  fi
  if eval "$_line" 2>/dev/null; then
    _n=$((_n + 1))
  else
    _nbad=$((_nbad + 1))
  fi
done < "$BASE_CMDS"
assert "44 baseline：每一步都是單一命令且執行成功（bad=$_nbad）" test "$_nbad" -eq 0
assert "44 baseline：成功執行的步數＝script 導出的步數（$_n vs $STEPS_BASE）" \
  test "$_n" -eq "$STEPS_BASE"

# baseline 的答案（全部從步驟輸出機械萃取）：
#   dead owner  → list --long 的 OWNER 欄為 owner-dead
#   orphaned    → WHERE 欄為 dead 但 OWNER 不是 owner-dead（owner 還活著）
#   blocked     → 哪一個 <worker>.screen 命中權限框特徵（檔名即答案）
B44_DEAD="$(awk -F'\t' 'NR>1 && $6=="owner-dead" {print $1}' "$P4OUT/list-long.txt")"
B44_ORPH="$(awk -F'\t' 'NR>1 && $5=="dead" && $6!="owner-dead" {print $1}' "$P4OUT/list-long.txt")"
B44_BLOCK=""
for _f in "$P4OUT"/*.screen; do
  if grep -qF "Do you want to " "$_f" && grep -qF "Esc to cancel" "$_f"; then
    B44_BLOCK="$(basename "$_f" .screen)"
  fi
done
printf '%s\n' "$B44_DEAD" "$B44_ORPH" "$B44_BLOCK" | sort > "$P4OUT/baseline.answer"
assert "44 baseline：三個異常 id 全對（正確率 100%）" \
  bash -c 'printf "p4w06\np4w09\np4w12\n" | diff -q - "$1"' _ "$P4OUT/baseline.answer"

# ---- TUI replay ----
tmx send-keys -t "$TUI44" \
  "$(printf 'echo AB44-MAIN-SCREEN; touch %q' "$TESTROOT/tui44-ready")" Enter
wait_for 10 test -f "$TESTROOT/tui44-ready"
tmx send-keys -t "$TUI44" \
  "$(printf 'env AGENT_BRIDGE_DATA=%q %q ui' "$D44" "$BRIDGE")" Enter
cap44() { tmx capture-pane -p -t "$TUI44" > "$P4OUT/tui-step$1.cap" 2>/dev/null; }
# shellcheck disable=SC2329  # 經 wait_for 間接呼叫
ui44_ready() { cap44 boot && grep -qF "p4w01" "$P4OUT/tui-stepboot.cap" 2>/dev/null; }
# shellcheck disable=SC2329  # 經 wait_for 間接呼叫
ui44_blocker_ready() { cap44 boot && grep -qF "blocked" "$P4OUT/tui-stepboot.cap"; }
assert "44 TUI：UI 起得來" wait_for 10 ui44_ready
# BLOCKER 軸的第一輪查詢（tmux capture-pane ×N，走背景 worker）要跑完才算
# 「首頁畫面」——步 0 的兩個異常之一就靠它
assert "44 TUI：BLOCKER 軸第一輪查詢已落地（步 0 的前提）" \
  wait_for 15 ui44_blocker_ready
cap44 0

_k=0
_kbad=0
while IFS= read -r _key; do
  [[ "$_key" =~ ^[[:space:]]*(#|$) ]] && continue
  if ! p4_key_ok "$_key"; then
    _kbad=$((_kbad + 1))
    continue
  fi
  if tmx send-keys -t "$TUI44" "$_key" 2>/dev/null; then
    _k=$((_k + 1))
  else
    _kbad=$((_kbad + 1))
  fi
  sleep 0.6
  cap44 "$_k"
done < "$TUI_KEYS"
assert "44 TUI：每一步都是合法單鍵且送達成功（bad=$_kbad）" test "$_kbad" -eq 0
assert "44 TUI：成功送出的按鍵數＝script 導出的步數（$_k vs $STEPS_TUI）" \
  test "$_k" -eq "$STEPS_TUI"

# ---- TUI 的成功 marker：canonical answer，逐幀綁定 ----
# 只做 substring 命中不夠：`p4w12` 與「它的 owner 已死」必須來自**同一幀**
# （不同幀分開 grep 等於允許兩個互不相干的畫面湊出一個結論），而且不能只驗
# 「有命中」——多餘的誤標也必須讓斷言紅（codex 複核 major #3）。
#
# 做法：逐幀抽事實三元組 `<worker>\t<mark>\t<origin 標籤>`——
#   - origin 取該幀 ORIGINS 欄帶 `▸` 的那一列。**P4.6 之後這一列不再有 ●／✗
#     glyph**（window 死活不再冒充 agent 死活）；window 已消失的事實改由標籤
#     自己說：`session:@id (gone)`。origin 列 `▸ <session>:<…>` 與 worker 列
#     `▸ p4wNN`／`ALL` 列天然分得開（worker 名與 ALL 都不含 `:`）
#   - worker 列以 `▸ p4wNN` 起頭、切到欄邊框 `│` 為止（DETAIL 欄也會出現
#     `blocked` 字樣，不切欄就會把它算進來）
# 然後**只留下異常**（有標記，或 owner 已死）去重排序，與期望的三行 exact-diff。
p4_frame_facts() {
  local f="$1" origin line name mark
  # 邊框字元**兩種都要當終止符**：focus 的面板畫的是粗框 `┃`，只擋 `│` 的話
  # ORIGINS 一聚焦，這個擷取就會一路吃進 WORKERS 欄
  origin="$(grep -oE '▸ [a-z]+:[^│┃]*' "$f" | head -1 \
    | sed -e 's/^▸ //' -e 's/[[:space:]]*$//')"
  [[ -z "$origin" ]] && return 0
  while IFS= read -r line; do
    name="$(printf '%s' "$line" | grep -oE 'p4w[0-9]{2}' | head -1)"
    [[ -z "$name" ]] && continue
    mark=none
    case "$line" in
      *blocked*) mark=blocked ;;
      *'✗dead'*) mark=dead ;;
      *copy-mode*) mark=occluded ;;
    esac
    printf '%s\t%s\t%s\n' "$name" "$mark" "$origin"
    # 同上：worker 列的終止符也必須含粗框。WORKERS 一聚焦，右框是 `┃`，
    # 只擋 `│` 的話這一段會一路吃到 DETAIL 欄的左框——而 DETAIL 正好會投影
    # 出 `blocked`／`copy-mode` 字樣，於是憑空長出一個假標記
  done < <(grep -oE '▸ p4w[0-9]{2}[^│┃]*' "$f")
}

# parser regression：worker 列的擷取 MUST 終止在 **WORKERS 面板自己的右框**，
# 不論它是細框還是粗框（聚焦時是 `┃`）。
#
# 誠實記錄適用範圍：現行三欄版面上 `┃` 後面緊接著就是 DETAIL 的細框 `│`
# （DETAIL 永不聚焦），所以舊規則 `[^│]*` 實際上也停得住、量不出假事實
# ——這條鎖的是**規則的意圖**，不是一個現存的可觸發缺陷。版面一旦變動
# （DETAIL 可聚焦、或兩欄之間多出任何字），舊規則就會把隔壁欄的
# `blocked`／`copy-mode` 算進來，而那是靜默的假事實。
# 本條不碰 TUI、純驗 parser，故不計入 P4 步數。
P4PARSE="$P4OUT/parser-regression.cap"
printf '│  ▸ zzq:fake  ┃  ▸ p4w77  codex  %%1  ready   ┃ blocker: blocked\n' \
  > "$P4PARSE"
p4_frame_facts "$P4PARSE" > "$P4OUT/parser-regression.out"
printf 'p4w77\tnone\tzzq:fake\n' > "$P4OUT/parser-regression.expect"
assert "44 parser：worker 列擷取終止於自己的右框（粗框亦然），不吃隔壁欄" \
  diff -q "$P4OUT/parser-regression.expect" "$P4OUT/parser-regression.out"
: > "$P4OUT/tui.facts"
for _f in "$P4OUT"/tui-step[0-9]*.cap; do
  p4_frame_facts "$_f" >> "$P4OUT/tui.facts"
done
# 異常＝worker 自己帶標記，或它的 origin window 已消失。其餘（正常 worker）
# 一律不得出現在答案裡——誤標會多一行，漏標會少一行，兩邊都紅
awk -F'\t' '$2 != "none" || $3 ~ /\(gone\)/' "$P4OUT/tui.facts" | sort -u \
  > "$P4OUT/tui.answer"
# p4w12 的 pane 隨著它的 origin window 一起消失，所以它自己也帶 dead 標記——
# 「worker 的 pane 死了」與「它的 origin window 沒了」是兩個獨立的軸，這一行
# 同時滿足兩者（orphan 的 p4w09 則是 pane 死、origin window 還在）。
# origin 欄是 P4.6 的誠實標籤：window 活著顯示 `session:window-name`
# （session 取當下查到的真值），消失則保留原 `session:@id` 並標 (gone)
printf '%s\t%s\t%s\n' \
  p4w06 blocked "it:$W44ANAME" \
  p4w09 dead "it:$W44BNAME" \
  p4w12 dead "$OWN44C (gone)" | sort > "$P4OUT/tui.expect"
printf '  [P4] TUI canonical answer：\n'
sed 's/^/    /' "$P4OUT/tui.answer"
assert "44 TUI：三個異常的 canonical answer 逐幀綁定、無多餘命中" \
  diff -q "$P4OUT/tui.expect" "$P4OUT/tui.answer"

# 44g：BLOCKER 軸的 occluded 分支（§4 v1 契約的第二項，結構性 `pane_in_mode`）。
# **不屬 P4 的步數量測**（按鍵在 replay script 之外，步數斷言此時已完成）：
# 這是上一輪自己標記的覆蓋缺口——occlusion 先前只有單元測試，沒有畫面驗證。
# 手法沿用分組 39（真 copy-mode，不是模擬）。
tmx send-keys -t "$TUI44" k
tmx send-keys -t "$TUI44" k   # ORIGINS 欄回到 origin A（p4w01 在它底下）
tmx copy-mode -t "$P4P01"
tmx display -pt "$P4P01" '#{pane_in_mode}' > "$TESTROOT/p44-mode.txt" 2>/dev/null
assert "44g 前置：pane 確實進入 copy-mode（不驗這條可能在沒進 mode 的畫面上假綠）" \
  grep -Fxq 1 "$TESTROOT/p44-mode.txt"
# shellcheck disable=SC2329  # 經 wait_for 間接呼叫
ui44_occluded() {
  tmx capture-pane -p -t "$TUI44" > "$P4OUT/tui-occl.cap" 2>/dev/null \
    && grep -qE 'p4w01.*copy-mode' "$P4OUT/tui-occl.cap"
}
assert "44g BLOCKER 軸 occluded：copy-mode 的 worker 在畫面上標 copy-mode" \
  wait_for 15 ui44_occluded
# copy-mode 是「人正在看」，不是「worker 被框住」——兩者 MUST NOT 混為一談
assert "44g occluded MUST NOT 被寫成 blocked" \
  bash -c '! grep -qE "p4w01.*blocked" "$1"' _ "$P4OUT/tui-occl.cap"
tmx send-keys -t "$P4P01" -X cancel 2>/dev/null || true

# ---- gate ----
printf '  [P4] baseline 步數＝%s、TUI 步數＝%s（比值 %s%%；gate ≤50%%）\n' \
  "$STEPS_BASE" "$STEPS_TUI" "$((STEPS_TUI * 100 / STEPS_BASE))"
assert "44 gate：TUI 步數 ≤ baseline 的 50%（$STEPS_TUI vs $STEPS_BASE）" \
  test "$((STEPS_TUI * 2))" -le "$STEPS_BASE"

tmx send-keys -t "$TUI44" q
tmx kill-window -t "$W44A" 2>/dev/null || true
tmx kill-window -t "$W44P" 2>/dev/null || true
tmx kill-window -t "$W44B" 2>/dev/null || true

else
  printf 'SKIP: 44 P4 效率驗收（SRC_KIND=bash：bash 正本凍結在 M4，不含 TUI）\n'
fi

# ---- 總結 ----
printf '\n共 %d PASS、%d FAIL\n' "$PASS" "$FAIL"
if (( FAIL > 0 )); then
  exit 1
fi
exit 0
