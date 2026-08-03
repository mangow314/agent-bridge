#!/usr/bin/env bash
# tests/pg4-fixture.sh — PG4 理解驗收（tui-design §9 rubric v2 page 層）的量測載具
#
# PG4 是 page 層唯一剩下的 human judgment 相位：受測者**只看通知、不開任何面板**
# （不跑 ui、不看 list、不讀事件流），要能對每一則說出「哪個 agent、為何現在
# 需要我」。agent 不能自證這一條，這支腳本只負責：
#   1. 把 10 個混合事件依序立起來（5 個該推的正例 ＋ 5 個不該推的負例）；
#   2. 讓該推的那 5 則走**真的桌面通知**（Hyprland notify-send），同時抄一份
#      進日誌，事後才有得查核；
#   3. 機械證明「推了幾則、推的是哪幾則、哪幾步一則都沒推」逐條對得上——
#      這是 rubric 裡機器判得動的那一半，判不動的那一半由受測者回答。
#
# **為什麼要隔離**：資料目錄與 tmux socket 都是本載具自己的（$STATE 與 -L
# agent-bridge-pg4），量測不會碰到使用者真正的 worker 池。桌面通知是唯一與
# 外界共用的通道——那正是要量的東西。
#
# 事件表（`run` 逐步執行，每步之間停 $PG4_PACE 秒）：
#   步驟 1  正例  fail T1        → task-failed（pg4-alpha，pane 活著）
#   步驟 2  正例  fail T2        → task-failed（pg4-beta，delivered 也能 fail）
#   步驟 3  正例  scan           → worker-died（pg4-gamma 第 1 代，running task）
#   步驟 4  正例  scan           → worker-died（pg4-delta，delivered task）
#   步驟 5  正例  scan           → worker-died（pg4-gamma **第 2 代**：同名
#                                  respawn 是另一個事實，不得被去重吃掉）
#   步驟 6  負例  reply T6       → completed 終態零推播
#   步驟 7  負例  scan           → pane 死了但 task 已 completed（pg4-echo）
#   步驟 8  負例  scan           → pane 死了但沒掛任何 task（pg4-zeta）
#   步驟 9  負例  scan           → pane 活著且 task running（pg4-eta）
#   步驟 10 負例  scan           → 步驟 3-5 的同一批事實重掃，去重不得重推
#
# 用法：
#   tests/pg4-fixture.sh up      建立載具（**不推任何通知**），印量測指引
#   tests/pg4-fixture.sh run     跑那 10 步（受測者此時只看通知）
#   tests/pg4-fixture.sh check   跑機械不變式（run 之後才有意義）
#   tests/pg4-fixture.sh log     印日誌（**含答案，受測者作答前不要看**）
#   tests/pg4-fixture.sh down    拆掉 socket 與資料目錄
#
# 環境變數：PG4_PACE（每步間隔秒數，預設 8）
#           PG4_DESKTOP（0＝只寫日誌不彈桌面通知，載具乾跑用；預設 1）
# shellcheck disable=SC2016  # $1/$2/$3 由內層 bash 展開，刻意單引號
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BRIDGE="${BRIDGE:-$ROOT/bin/ab}"
SOCK="agent-bridge-pg4"
STATE="${TMPDIR:-/tmp}/agent-bridge-pg4"
DATA="$STATE/data"
SHIM="$STATE/shim"
LOG="$STATE/notify.log"
NOTIFIER="$STATE/pg4-notify"
ENVFILE="$STATE/env"
MARKER="$STATE/.pg4-fixture"
PACE="${PG4_PACE:-8}"
DESKTOP="${PG4_DESKTOP:-1}"

# 從載具 pane 內重跑時 PATH 最前面是 $SHIM，`command -v tmux` 會解析到 shim
# 自己而自指掛死（p5-fixture 踩過）。逐項掃 PATH、跳過 shim
REAL_TMUX="$(type -aP tmux | grep -vF "$SHIM/" | head -1)"
[[ -n "$REAL_TMUX" ]] || { echo "錯誤：PATH 上找不到真正的 tmux" >&2; exit 1; }

tmx() { "$REAL_TMUX" -L "$SOCK" -f /dev/null "$@"; }

# 所有 ab 呼叫都走這一支：資料目錄、自訂 notifier、shim tmux 一次到位。
# **shim 是必要的**：`scan` 問的是「哪些 pane 還活著」，不導到本載具的 socket
# 就會拿使用者真正的 tmux server 當 oracle
ab() {
  env AGENT_BRIDGE_DATA="$DATA" AGENT_BRIDGE_NOTIFY_CMD="$NOTIFIER" \
      PATH="$SHIM:$PATH" "$BRIDGE" "$@"
}

PASS=0
FAIL=0
ok()  { PASS=$((PASS + 1)); printf 'PASS: %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf 'FAIL: %s\n' "$1"; }
assert() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then ok "$desc"; else bad "$desc"; fi
}

# ---- 形狀常數 ----
REG_AT="2026-08-01T00:00:00Z"           # 第 1 代 agent 的註冊時間
TASK_AT="2026-08-02T00:00:00Z"          # 第 1 代的 task（晚於註冊＝掛得上）
REG_AT2="2026-08-02T12:00:00Z"          # pg4-gamma 第 2 代
TASK_AT2="2026-08-02T13:00:00Z"

T1="20260802T000001Z-aa01"   # pg4-alpha  running    → 步驟 1 fail
T2="20260802T000002Z-aa02"   # pg4-beta   delivered  → 步驟 2 fail
T3="20260802T000003Z-aa03"   # pg4-gamma  running    → 步驟 3 worker-died
T4="20260802T000004Z-aa04"   # pg4-delta  delivered  → 步驟 4 worker-died
T5="20260802T130000Z-aa05"   # pg4-gamma  running    → 步驟 5 worker-died（第 2 代）
T6="20260802T000006Z-aa06"   # pg4-eta    running    → 步驟 6 reply（負例）
T7="20260802T000007Z-aa07"   # pg4-echo   completed  → 步驟 7 負例
T9="20260802T000009Z-aa09"   # pg4-eta    running    → 步驟 9 負例

tag() { printf 'AGENT_BRIDGE_SPAWN_TAG=ab-spawn-%s-%s-%s' "$1" "$2" "$3"; }
TAG_ALPHA="$(tag pg4-alpha 201 a1a1a1a1a1a1)"
TAG_BETA="$(tag pg4-beta 202 b2b2b2b2b2b2)"
TAG_GAMMA1="$(tag pg4-gamma 203 c3c3c3c3c3c3)"
TAG_GAMMA2="$(tag pg4-gamma 204 d4d4d4d4d4d4)"
TAG_DELTA="$(tag pg4-delta 205 e5e5e5e5e5e5)"
TAG_ECHO="$(tag pg4-echo 206 f6f6f6f6f6f6)"
TAG_ZETA="$(tag pg4-zeta 207 070707070707)"
TAG_ETA="$(tag pg4-eta 208 080808080808)"

# 死掉的 pane id：本載具 socket 上不存在的號碼（scan 的 oracle 是
# `list-panes -a`，所以「不在清單裡」＝死了）
DEAD_GAMMA1="%94401"
DEAD_GAMMA2="%94402"
DEAD_DELTA="%94403"
DEAD_ECHO="%94404"
DEAD_ZETA="%94405"

# 預期會被推播的 5 個 event key（`page::PageEvent::key` 的形狀）
expected_keys() {
  printf 'failed:%s\n' "$T1"
  printf 'failed:%s\n' "$T2"
  printf 'died:pg4-gamma:%s:%s\n' "$TAG_GAMMA1" "$T3"
  printf 'died:pg4-delta:%s:%s\n' "$TAG_DELTA" "$T4"
  printf 'died:pg4-gamma:%s:%s\n' "$TAG_GAMMA2" "$T5"
}

# ---- 資料寫入（一律直接寫檔，不經 CLI）----
# **刻意不用 CLI 建資料**：每個非唯讀子指令進場都會順手掃一輪，用 CLI 佈景會
# 在佈景階段就把事件推掉，10 步的歸屬就對不上了。
# `owner` 寫**真的 window id**（`$OWNER` 由 up 算出並存進 ENVFILE）：worker 死
# 掉時 pane 已不在，地點只剩 owner 的 window 這一條線索（CLI-PAGE-3 的
# fallback）。寫一個不存在的 window id 會讓死亡通知永遠沒有地點——量到的是
# 載具的破綻，不是被測物的
mkagent() { # mkagent <name> <pane> <spawn_tag> [registered_at]
  printf '{"name":"%s","pane_id":"%s","registered_at":"%s","spawned":true,"ready":true,"runtime":"codex","spawn_tag":"%s","owner":"%s"}\n' \
    "$1" "$2" "${4:-$REG_AT}" "$3" "$OWNER" > "$DATA/agents/$1.json"
}

mktask() { # mktask <task-id> <to> <status> <created_at>
  mkdir -p "$DATA/tasks/$1"
  printf '%s\n' "$3" > "$DATA/tasks/$1/status"
  printf 'pg4 fixture request\n' > "$DATA/tasks/$1/request.md"
  printf '{"version":1,"task_id":"%s","from":"pg4-boss","to":"%s","created_at":"%s","updated_at":"%s","working_directory":"/tmp","status":"%s"}\n' \
    "$1" "$2" "$4" "$4" "$3" > "$DATA/tasks/$1/metadata.json"
}

# ---- 不變式 ----
notify_lines() { grep -c '^NOTIFY' "$LOG" 2>/dev/null || true; }

# 每一步推了幾則：把 STEP 標記與其後的 NOTIFY 行配對
step_counts() {
  awk -F'\t' '
    /^STEP/ { step = $2; c[step] = c[step] + 0; next }
    /^NOTIFY/ { c[step]++ }
    END { for (s = 1; s <= 10; s++) printf "%d\t%d\n", s, c[s] }
  ' "$LOG"
}

run_checks() {
  PASS=0; FAIL=0
  [[ -s "$LOG" ]] || { echo "錯誤：找不到 $LOG，請先跑 up 再跑 run" >&2; return 2; }

  # 1) 總量：恰 5 則推播、恰 5 筆落盤
  assert "恰 5 則通知（5 正例全推、5 負例全不推）" \
    test "$(notify_lines)" -eq 5
  assert "事件流恰 5 筆（推播與落盤一致）" \
    bash -c 'test "$(wc -l < "$1")" -eq 5' _ "$DATA/state/page-events.jsonl"

  # 2) 逐則：event key 集合逐字相符（只數數量會讓「推了 5 則但推錯了」通過）
  expected_keys | sort > "$STATE/keys.expect"
  jq -r .key "$DATA/state/page-events.jsonl" 2>/dev/null | sort > "$STATE/keys.actual"
  assert "5 個 event key 逐字相符（含 gamma 兩代各一則）" \
    diff -q "$STATE/keys.expect" "$STATE/keys.actual"

  # 3) 歸屬：正例 1-5 各恰一則、負例 6-10 一則都沒有
  local _s _c
  while read -r _s _c; do
    if (( _s <= 5 )); then
      assert "步驟 $_s（正例）恰推 1 則" test "$_c" -eq 1
    else
      assert "步驟 $_s（負例）零推播" test "$_c" -eq 0
    fi
  done < <(step_counts)

  # 4) 通知文字本身答得出「哪個 agent、哪一筆 task」——受測者不開面板的前提。
  # **只掃 NOTIFY 行**：STEP 標記行寫著每一步的正解（含負例的 agent 名），拿
  # 整份日誌當標的的話，正例會被標記行餵成假綠、負例會被它誣成假紅
  local NOTES="$STATE/notify-only.log"
  grep '^NOTIFY' "$LOG" > "$NOTES" || true
  local _a _t
  while read -r _a _t; do
    [[ -z "$_a" ]] && continue
    assert "通知含 agent 名 $_a 與 task $_t" \
      bash -c 'grep -F "$2" "$1" | grep -qF "$3"' _ "$NOTES" "$_a" "$_t"
  done <<EOF
pg4-alpha $T1
pg4-beta $T2
pg4-gamma $T3
pg4-delta $T4
pg4-gamma $T5
EOF

  # 4b) CLI-PAGE-3：說得出「切去哪裡」與「要不要出手」——PG4 第二輪打回來的
  # 兩個缺口。**五則都要有地點**，包括 worker-died 那三則（它們的 pane 已死，
  # 地點只能從 owner 的 window 解出來，最容易靜默地少一段）
  # shellcheck disable=SC1090  # up 產生的位置檔
  . "$ENVFILE"
  assert "五則通知全帶地點 $LOC（含 pane 已死的三則）" \
    bash -c 'test "$(grep -cF "$2" "$1")" -eq 5' _ "$NOTES" "$LOC"
  assert "失敗通知帶原因而非狀態字（步驟 1）" \
    grep -qF "編譯失敗：缺 libfoo" "$NOTES"
  assert "失敗通知帶原因而非狀態字（步驟 2）" \
    grep -qF "測試 3 紅：逾時" "$NOTES"

  # 5) 負例的名字一個都不准出現在通知裡
  assert "pg4-echo（死 pane＋終態 task）MUST NOT 出現在通知" \
    bash -c '! grep -q "pg4-echo" "$1"' _ "$NOTES"
  assert "pg4-zeta（死 pane＋無 task）MUST NOT 出現在通知" \
    bash -c '! grep -q "pg4-zeta" "$1"' _ "$NOTES"
  assert "pg4-eta（活 pane＋running task）MUST NOT 出現在通知" \
    bash -c '! grep -q "pg4-eta" "$1"' _ "$NOTES"
  assert "已完成的 $T6 MUST NOT 出現在通知" \
    bash -c '! grep -q "$2" "$1"' _ "$NOTES" "$T6"

  # 6) 載具本身沒退化：死 pane 真的死、活 pane 真的活
  assert "pg4-eta 的 pane 存活（負例 9 的前提）" \
    bash -c 'tmx_out=$("$1" -L "$2" list-panes -a -F "#{pane_id}"); printf "%s\n" "$tmx_out" | grep -qFx "$3"' \
    _ "$REAL_TMUX" "$SOCK" "$(sed -n 's/^P_ETA=//p' "$ENVFILE" | tr -d "'")"
  assert "五個死 pane 全不在 tmux 清單上" \
    bash -c '! "$1" -L "$2" list-panes -a -F "#{pane_id}" | grep -qE "^(%94401|%94402|%94403|%94404|%94405)$"' \
    _ "$REAL_TMUX" "$SOCK"

  printf '\nPG4 機械不變式：%d PASS %d FAIL\n' "$PASS" "$FAIL"
  test "$FAIL" -eq 0
}

# ---- 生命週期 ----
do_down() {
  local rc=0
  tmx kill-server 2>/dev/null || true
  if [[ -d "$STATE" ]]; then
    if [[ -e "$MARKER" ]]; then
      find "$STATE" -mindepth 1 -delete 2>/dev/null
      rmdir "$STATE" 2>/dev/null || true
    else
      echo "錯誤：$STATE 缺 ownership marker（.pg4-fixture），非本載具所建，拒絕刪除" >&2
      rc=2
    fi
  fi
  echo "PG4 載具已拆除（socket $SOCK、資料 $STATE）"
  return "$rc"
}

do_up() {
  [[ -x "$BRIDGE" ]] || {
    echo "錯誤：找不到可執行的 $BRIDGE（先 cargo build --release && cp -f target/release/ab bin/ab）" >&2
    exit 1
  }
  do_down >/dev/null || { echo "錯誤：舊環境未能乾淨拆除，拒絕 up（見上方 stderr）" >&2; exit 1; }
  mkdir -p "$DATA/agents" "$DATA/tasks" "$DATA/state" "$SHIM"
  : > "$MARKER"

  printf '#!/usr/bin/env bash\nunset TMUX\nexec %q -L %q -f /dev/null "$@"\n' \
    "$REAL_TMUX" "$SOCK" > "$SHIM/tmux"
  chmod +x "$SHIM/tmux"

  # notifier：先抄一份進日誌，再真的彈桌面通知。
  # **argv 契約是 `<prog> <title> <body>`**（page::SystemPager::page_desktop），
  # 值是 argv[0]＝一支可執行檔，不是 shell 字串
  # `PG4_DESKTOP=0` 只寫日誌不彈通知：載具自己的乾跑用，免得把答案先閃給受測者
  # 日誌**一則一行**：內文自 CLI-PAGE-3 起是多行（首行原因、末行 task id），
  # 原樣寫進去會把一則通知拆成兩行，逐則斷言就對不起來。換行壓成 ` ⏎ `——
  # 壓平的只是日誌，真正送出去的 `$2` 原樣不動
  printf '#!/usr/bin/env bash\nprintf "NOTIFY\\t%%s\\t%%s\\n" "$1" "${2//$'"'"'\\n'"'"'/ ⏎ }" >> %q\n[[ "%s" == 1 ]] && command -v notify-send >/dev/null && notify-send -a agent-bridge "$1" "$2"\nexit 0\n' \
    "$LOG" "$DESKTOP" > "$NOTIFIER"
  chmod +x "$NOTIFIER"
  [[ "$DESKTOP" == 1 ]] && ! command -v notify-send >/dev/null && \
    echo "警告：找不到 notify-send，這一輪只會進日誌、不會有桌面通知" >&2

  local pane_cmd
  pane_cmd="$(printf 'env AGENT_BRIDGE_DATA=%q PATH=%q bash --norc --noprofile' \
    "$DATA" "$SHIM:$PATH")"
  tmx new-session -d -s pg4 -x 120 -y 40 "$pane_cmd"

  local P_ALPHA P_BETA P_ETA OWNER LOC
  P_ALPHA="$(tmx list-panes -t pg4 -F '#{pane_id}' | head -1)"
  P_BETA="$(tmx split-window -dP -F '#{pane_id}' -t pg4 "$pane_cmd")"
  P_ETA="$(tmx split-window -dP -F '#{pane_id}' -t pg4 "$pane_cmd")"
  # 早退：server 沒起來時這些值全空，而空字串的斷言會**通過**——載具會靜默地
  # 變成「全部 pane 都死了」，5 個負例跟著失去標的
  [[ "$P_ALPHA" == %* && "$P_BETA" == %* && "$P_ETA" == %* ]] || {
    echo "錯誤：tmux server 未就緒（$P_ALPHA / $P_BETA / $P_ETA）" >&2; exit 1
  }
  OWNER="$(tmx display-message -p -t "$P_ALPHA" '#{session_name}:#{window_id}')"
  LOC="$(tmx display-message -p -t "$P_ALPHA" '#{session_name}:#{window_index}')"
  [[ "$OWNER" == *:@* ]] || { echo "錯誤：算不出 owner（$OWNER）" >&2; exit 1; }

  # 正例的兩個 agent：pane 活著（它們推的是 task-failed，與死活無關）
  mkagent pg4-alpha "$P_ALPHA" "$TAG_ALPHA"
  mkagent pg4-beta  "$P_BETA"  "$TAG_BETA"
  # 負例：pane 活著且 task running（最常見的正常狀態，最不該被打擾）
  mkagent pg4-eta   "$P_ETA"   "$TAG_ETA"
  # 負例：pane 死了但 task 已終態／根本沒 task
  mkagent pg4-echo  "$DEAD_ECHO" "$TAG_ECHO"
  mkagent pg4-zeta  "$DEAD_ZETA" "$TAG_ZETA"

  mktask "$T1" pg4-alpha running   "$TASK_AT"
  mktask "$T2" pg4-beta  delivered "$TASK_AT"
  mktask "$T6" pg4-eta   running   "$TASK_AT"
  mktask "$T7" pg4-echo  completed "$TASK_AT"
  mktask "$T9" pg4-eta   running   "$TASK_AT"

  # gamma／delta 的 agent 與 task **不在 up 建**：它們一出現就是可推播的事實，
  # 而任何一個非唯讀子指令都會順手把它推掉。改由 run 在該步之前才立起來
  printf 'P_ALPHA=%q\nP_BETA=%q\nP_ETA=%q\nOWNER=%q\nLOC=%q\n' \
    "$P_ALPHA" "$P_BETA" "$P_ETA" "$OWNER" "$LOC" > "$ENVFILE"
  : > "$LOG"

  cat <<EOF

PG4 量測載具就緒（資料 $DATA、socket $SOCK）。

接著跑：

    tests/pg4-fixture.sh run

會依序立起 10 個事件，每步間隔 ${PACE}s（PG4_PACE 可調），全程約 $((PACE * 10))s。

受測者守則（rubric 的前提，破了這一條這一輪就作廢）：
  **只看桌面通知**——期間不要跑 agent-bridge ui／list、不要看事件流、
  不要看本腳本的日誌（日誌含答案）。

量完請逐條回答：
  1. 每一則都說得出是「哪個 agent」嗎？
  2. 每一則都說得出事件類別與 task id 嗎？
  3. 不開任何面板就判斷得出「要不要現在出手」嗎？
  4. 這一輪有沒有「不需要行動卻推了」的雜訊？

作答後再跑 tests/pg4-fixture.sh check（機械那一半）與 log（對答案）。
拆除：tests/pg4-fixture.sh down
EOF
}

step() { # step <n> <描述（只進日誌，不印給受測者看）>
  printf 'STEP\t%s\t%s\n' "$1" "$2" >> "$LOG"
  printf '  步驟 %s/10\n' "$1"
}

do_run() {
  [[ -s "$ENVFILE" ]] || { echo "錯誤：找不到 $ENVFILE，請先跑 up" >&2; exit 2; }
  # shellcheck disable=SC1090  # up 產生的位置檔（pane / owner id）
  . "$ENVFILE"
  [[ -s "$DATA/state/page-events.jsonl" ]] && {
    echo "錯誤：這一輪已經跑過了（事件流非空）。去重會讓重跑全部靜音——請先重跑 up" >&2
    exit 2
  }

  echo "開始（受測者請只看桌面通知，不要開任何面板）"
  sleep 2

  step 1 "正例 task-failed pg4-alpha $T1"
  ab fail "$T1" --message "編譯失敗：缺 libfoo" >/dev/null 2>&1
  sleep "$PACE"

  step 2 "正例 task-failed pg4-beta $T2"
  ab fail "$T2" --message "測試 3 紅：逾時" >/dev/null 2>&1
  sleep "$PACE"

  # gamma 第 1 代：立起來之後才掃
  mkagent pg4-gamma "$DEAD_GAMMA1" "$TAG_GAMMA1"
  mktask "$T3" pg4-gamma running "$TASK_AT"
  step 3 "正例 worker-died pg4-gamma gen1 $T3"
  ab scan >/dev/null 2>&1
  sleep "$PACE"

  mkagent pg4-delta "$DEAD_DELTA" "$TAG_DELTA"
  mktask "$T4" pg4-delta delivered "$TASK_AT"
  step 4 "正例 worker-died pg4-delta $T4"
  ab scan >/dev/null 2>&1
  sleep "$PACE"

  # gamma 第 2 代：同名、新 spawn_tag、新 registered_at（第 1 代的 $T3 因此
  # 早於這一代的註冊時間＝不再掛在這一代身上，只有 $T5 掛得上）
  mkagent pg4-gamma "$DEAD_GAMMA2" "$TAG_GAMMA2" "$REG_AT2"
  mktask "$T5" pg4-gamma running "$TASK_AT2"
  step 5 "正例 worker-died pg4-gamma gen2 $T5（同名 respawn 是另一個事實）"
  ab scan >/dev/null 2>&1
  sleep "$PACE"

  step 6 "負例 reply→completed $T6"
  ab reply "$T6" --message "做完了" >/dev/null 2>&1
  sleep "$PACE"

  step 7 "負例 死 pane＋終態 task pg4-echo $T7"
  ab scan >/dev/null 2>&1
  sleep "$PACE"

  step 8 "負例 死 pane＋無 task pg4-zeta"
  ab scan >/dev/null 2>&1
  sleep "$PACE"

  step 9 "負例 活 pane＋running task pg4-eta $T9"
  ab scan >/dev/null 2>&1
  sleep "$PACE"

  step 10 "負例 重掃步驟 3-5 的同一批事實（去重）"
  ab scan >/dev/null 2>&1

  cat <<'EOF'

10 步跑完。請先憑記憶回答 rubric 四問，再跑：

    tests/pg4-fixture.sh check    機械不變式
    tests/pg4-fixture.sh log      對答案（含正解，作答後再看）
EOF
}

do_log() {
  [[ -s "$LOG" ]] || { echo "錯誤：找不到 $LOG" >&2; exit 2; }
  awk -F'\t' '
    /^STEP/   { printf "\n步驟 %s：%s\n", $2, $3 }
    /^NOTIFY/ { printf "    ↑ 通知：%s ／ %s\n", $2, $3 }
  ' "$LOG"
}

case "${1:-up}" in
  up)    do_up ;;
  run)   do_run ;;
  check) run_checks ;;
  log)   do_log ;;
  down)  do_down ;;
  *) echo "用法: $0 {up|run|check|log|down}" >&2; exit 2 ;;
esac
