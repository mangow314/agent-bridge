#!/usr/bin/env bash
# tests/p5-fixture.sh — P5 理解驗收（§9）的量測環境
#
# P5 是 human judgment 相位：受測者要在 10 秒內畫出
# lineage → worker → in-flight task 的歸屬樹。agent 不能自證，這支腳本
# 只負責把「受測畫面」立起來，並機械證明它與 rubric 逐條對得上。
#
# 畫面＝分組 44 的 fixture ＋ P5 擴充列：
#   核心（與分組 44 逐字同形）：3 owner／12 worker／20 task／3 條 lineage
#     （6／5／1，其中 L3 的根與中間代缺席＝墓碑組標頭）／3 個異常。
#   P5 擴充（2026-08-02 使用者裁定）：rubric 條 1 後半與條 2 後半在原 fixture
#     上**沒有標的**（12 列全帶 canonical tag → 無 standalone 段；20 筆 task
#     全部可連結 → Unattached scope 為空），照原樣量等於這兩條空真通過。
#     故補：
#       - standalone 三形狀各一列：p4old(legacy)／p4man(manual)／p4bad(invalid)
#       - 連結不到的 in-flight task 三筆，涵蓋三種「不可證」：
#           u1 同名但 created_at 早於 registered_at（＝同名 respawn 不承接歷史 task）
#           u2 收件人根本不在 registry（p4ghost）
#           u3 同秒（created_at == registered_at）＝不可證＝不掛
#
# 這份與 tests/run-tests.sh 分組 44 是兩份獨立實作，所以核心部分重跑分組 44 的
# 全部前置不變式；擴充部分另有自己的不變式。**最後一道是畫面層斷言**：直接
# capture 起著的 TUI，確認 standalone 段與 unattached scope 真的在畫面上——
# 資料對不代表畫面對（分組 44 的 `tail_args` 教訓：假件只證明得了我方送出去的）。
# 任何一條紅就不給量。
#
# 用法：
#   tests/p5-fixture.sh up      建立 fixture 並在 tmux 裡起 TUI（印出 attach 指令）
#   tests/p5-fixture.sh down    拆掉整個 socket 與資料目錄
#   tests/p5-fixture.sh check   只重跑不變式（fixture 必須已 up）
# shellcheck disable=SC2016  # $1/$2/$r 由內層 bash/jq 展開，刻意單引號
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BRIDGE="${BRIDGE:-$ROOT/bin/ab}"
SOCK="agent-bridge-p5"
STATE="${TMPDIR:-/tmp}/agent-bridge-p5"
D44="$STATE/data"
SHIM="$STATE/shim"
ENVFILE="$STATE/env"
# 從 fixture pane 內重跑 up 時，PATH 最前面是 $SHIM——`command -v tmux` 會解析
# 到 shim 自己，產生一支自指 shim 而掛死。逐項掃 PATH、跳過 shim
REAL_TMUX="$(type -aP tmux | grep -vF "$SHIM/" | head -1)"
[[ -n "$REAL_TMUX" ]] || { echo "錯誤：PATH 上找不到真正的 tmux" >&2; exit 1; }

tmx() { "$REAL_TMUX" -L "$SOCK" -f /dev/null "$@"; }

PASS=0
FAIL=0
ok()  { PASS=$((PASS + 1)); printf 'PASS: %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf 'FAIL: %s\n' "$1"; }
assert() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then ok "$desc"; else bad "$desc"; fi
}
assert_fails() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then bad "$desc"; else ok "$desc"; fi
}
wait_for() {
  local timeout="$1"; shift
  local i
  for (( i = 0; i < timeout * 5; i++ )); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  "$@" >/dev/null 2>&1
}
pane_alive() { tmx list-panes -a -F '#{pane_id}' 2>/dev/null | grep -Fx "$1" >/dev/null; }
win_alive() { tmx list-windows -a -F '#{window_id}' 2>/dev/null | grep -Fx "$1" >/dev/null; }

# ---- fixture 的形狀常數（核心 12 列與分組 44 逐字對應）----
REG_AT="2026-07-31T00:00:00Z"
p4tag() { printf 'AGENT_BRIDGE_SPAWN_TAG=ab-spawn-%s-%s-%s' "$1" "$2" "$3"; }
P4T01="$(p4tag p4w01 101 a1a1a1a1a1a1)"
P4T02="$(p4tag p4w02 102 a2a2a2a2a2a2)"
P4T03="$(p4tag p4w03 103 a3a3a3a3a3a3)"
P4T04="$(p4tag p4w04 104 a4a4a4a4a4a4)"
P4T05="$(p4tag p4w05 105 a5a5a5a5a5a5)"
P4T06="$(p4tag p4w06 106 a6a6a6a6a6a6)"
P4T07="$(p4tag p4w07 107 b7b7b7b7b7b7)"
P4T08="$(p4tag p4w08 108 b8b8b8b8b8b8)"
P4T09="$(p4tag p4w09 109 b9b9b9b9b9b9)"
P4T10="$(p4tag p4w10 110 0a0a0a0a0a0a)"
P4T11="$(p4tag p4w11 111 0b0b0b0b0b0b)"
P4T12="$(p4tag p4w12 112 0c0c0c0c0c0c)"
P4L1="$P4T01"
P4L2="$P4T07"
P4L3="$(p4tag p4z9 119 c3c3c3c3c3c3)"
P4L3MID="$(p4tag p4z8 118 d4d4d4d4d4d4)"
# 擴充列：p4bad 宣稱的 root 刻意自成一格——它是 invalid，其 root MUST NOT
# 進 roots（model.rs `group_by_lineage` 第一趟），否則畫面會多出一個不存在的組
P4BADROOT="$(p4tag p4x0 120 e5e5e5e5e5e5)"
P4OLDTAG="t44-legacy-no-generation"

# ---- 不變式 ----
run_checks() {
  [[ -s "$ENVFILE" ]] || { echo "錯誤：找不到 $ENVFILE，請先跑 up" >&2; return 2; }
  # shellcheck disable=SC1090  # up 產生的位置檔（pane / owner id）
  . "$ENVFILE"

  # ---- 核心（與分組 44 同形）----
  # 核心 task 的 id 後綴是 -44a0..-44b3（160..179 的 hex），擴充的是 -44c0..-44c2
  assert "核心：worker 12 列、核心 task 20 筆" \
    bash -c 'test "$(ls "$1"/agents/p4w*.json | wc -l)" -eq 12 && test "$(ls "$1"/tasks | grep -cE -- "-44[ab][0-9a-f]$")" -eq 20' \
    _ "$D44"
  assert "擴充後總數：agent 15 列、task 23 筆" \
    bash -c 'test "$(ls "$1"/agents | wc -l)" -eq 15 && test "$(ls "$1"/tasks | wc -l)" -eq 23' \
    _ "$D44"

  jq -r '.owner' "$D44"/agents/p4w*.json | sort -u > "$STATE/owners.actual"
  printf '%s\n' "$OWN44A" "$OWN44B" "$OWN44C" | sort > "$STATE/owners.expect"
  assert "核心：owner 集合恰 3 個、逐字相符" \
    diff -q "$STATE/owners.expect" "$STATE/owners.actual"

  p4_owner_count() { jq -r --arg o "$2" 'select(.owner==$o) | .name' "$1"/agents/p4w*.json | wc -l; }
  while read -r _own _want; do
    [[ -z "$_own" ]] && continue
    assert "核心：owner $_own 底下恰 $_want 個 worker" \
      test "$(p4_owner_count "$D44" "$_own")" -eq "$_want"
  done <<EOF
$OWN44A 6
$OWN44B 5
$OWN44C 1
EOF

  local P4GENKEY='^AGENT_BRIDGE_SPAWN_TAG=ab-spawn-[A-Za-z0-9_-]+-[0-9]+-[0-9a-f]{12}$'
  p4_bad_tags() { jq -r '.spawn_tag' "$1"/agents/p4w*.json | grep -cvE "$P4GENKEY" || true; }
  assert "核心：12 列的 spawn_tag 全是 canonical generation key" \
    test "$(p4_bad_tags "$D44")" -eq 0

  jq -r '.lineage_root' "$D44"/agents/p4w*.json | sort -u > "$STATE/lineages.actual"
  printf '%s\n' "$P4L1" "$P4L2" "$P4L3" | sort > "$STATE/lineages.expect"
  assert "核心：lineage 集合恰 3 條、逐字相符（防「全 legacy」退化）" \
    diff -q "$STATE/lineages.expect" "$STATE/lineages.actual"

  p4_lineage_count() { jq -r --arg l "$2" 'select(.lineage_root==$l) | .name' "$1"/agents/p4w*.json | wc -l; }
  while read -r _lin _want; do
    [[ -z "$_lin" ]] && continue
    assert "核心：lineage ${_lin##*ab-spawn-} 底下恰 $_want 個 worker" \
      test "$(p4_lineage_count "$D44" "$_lin")" -eq "$_want"
  done <<EOF
$P4L1 6
$P4L2 5
$P4L3 1
EOF

  jq -r '.spawn_tag' "$D44"/agents/p4w*.json | sort > "$STATE/tags.actual"
  assert "核心：L3 的根與中間代不在 registry（墓碑組標頭／breadcrumb 的來源）" \
    bash -c '! grep -qxF "$2" "$1" && ! grep -qxF "$3" "$1"' _ \
    "$STATE/tags.actual" "$P4L3" "$P4L3MID"

  assert "核心：p4w12 的 pane 存活（擋 fixture 回退成雙異常）" pane_alive "$P4P12"
  assert_fails "核心：p4w09 的 pane 不存在（orphan 異常的來源）" pane_alive "%94409"
  assert_fails "核心：owner C 的 window 已消失（origin gone 異常的來源）" \
    win_alive "${OWN44C#*:}"

  # task 不能只數目錄名：單獨弄壞一份 metadata、或把一筆 running 改成
  # completed，目錄數都還是對的。逐份可解析＋status 分布整組鎖死
  assert "核心：23 份 metadata 全部可被 jq 解析" \
    bash -c 'jq -e . "$1"/tasks/*/metadata.json >/dev/null' _ "$D44"
  local _st _want
  while read -r _st _want; do
    [[ -z "$_st" ]] && continue
    assert "核心：status $_st 恰 $_want 筆（12 in-flight＋8 終態的分布）" \
      bash -c 'test "$(jq -r .status "$1"/tasks/*-44[ab][0-9a-f]/metadata.json | grep -cxF "$2")" -eq "$3"' \
      _ "$D44" "$_st" "$_want"
  done <<EOF
queued 4
running 6
delivered 2
completed 6
failed 1
cancelled 1
EOF

  p4_blocked_ready() {
    tmx capture-pane -p -t "$P4P06" 2>/dev/null | grep -qF "Esc to cancel"
  }
  assert "核心：blocked prompt 的畫面已就緒（異常 2／3 的來源）" \
    wait_for 15 p4_blocked_ready

  # ---- P5 擴充：standalone 三形狀（rubric 條 1 後半的標的）----
  # 形狀判準見 model.rs `row_shape`；這裡逐列驗它「說不出世代」的**理由**，
  # 只數 standalone 的列數會讓「三列全是 legacy」這種退化靜默通過
  assert "擴充：p4old＝legacy（spawn_tag 在、spawned、兩個 lineage 欄皆缺席）" \
    jq -e '.spawned == true and (.spawn_tag | length > 0) and (has("lineage_root") | not) and (has("parent_agent") | not)' \
    "$D44/agents/p4old.json"
  assert "擴充：p4man＝manual（無 spawn_tag、spawned:false、無 lineage 欄）" \
    jq -e '.spawned == false and (has("spawn_tag") | not) and (has("lineage_root") | not) and (has("parent_agent") | not)' \
    "$D44/agents/p4man.json"
  assert "擴充：p4bad＝invalid（有 lineage_root 卻 parent_agent 為空字串）" \
    jq -e '.lineage_root == $r and .parent_agent == ""' --arg r "$P4BADROOT" \
    "$D44/agents/p4bad.json"
  assert "擴充：p4bad 宣稱的 root 不被任何合法列使用（不得無中生有一個組）" \
    bash -c 'test "$(jq -r ".lineage_root // empty" "$1"/agents/p4w*.json | grep -cxF "$2")" -eq 0' \
    _ "$D44" "$P4BADROOT"
  # standalone 三列的 pane 必須活著：pane 若不在，它們同時變成新的 orphan，
  # 畫面上就會有第四、五、六個 ✗dead，rubric 條 3 的「恰 3 個異常」被稀釋
  local _p
  for _p in "$P4POLD" "$P4PMAN" "$P4PBAD"; do
    assert "擴充：standalone 列的 pane $_p 存活（不稀釋異常計數）" pane_alive "$_p"
  done

  # ---- P5 擴充：連結不到的 task（rubric 條 2 後半的標的）----
  # 判準單一事實源＝model.rs `attached()`：`to` 相符**且** created_at 嚴格 >
  # registered_at。三筆各打一種「不可證」
  assert "擴充：u1 同名 respawn 誘惑（to=p4w01，created_at 早於 registered_at）" \
    jq -e --arg r "$REG_AT" '.to == "p4w01" and .created_at < $r and .status == "running"' \
    "$D44/tasks/$U1/metadata.json"
  assert "擴充：u2 收件人不在 registry（p4ghost）" \
    bash -c 'jq -e ".to == \"p4ghost\"" "$1" >/dev/null && [[ ! -e "$2/agents/p4ghost.json" ]]' \
    _ "$D44/tasks/$U2/metadata.json" "$D44"
  assert "擴充：u3 同秒＝不可證＝不掛（created_at 等於 registered_at）" \
    jq -e --arg r "$REG_AT" '.to == "p4w07" and .created_at == $r' \
    "$D44/tasks/$U3/metadata.json"

  # ---- 畫面層：資料對不代表畫面對 ----
  # 先等 liveness 軸走完一輪：pane 死活與 blocker 走 tmux 輪詢（footer 標的
  # `tmux 2s`），磁碟那一輪（500ms）先到，於是有一個窗口是「列在了但標記還沒
  # 到」。抓得太早會把時序當成缺陷（實測踩過）
  tui_markers_ready() {
    local s
    s="$(tmx capture-pane -p -t "$TUI44" 2>/dev/null)" || return 1
    grep -qF "⛔" <<<"$s" && grep -qF "✗dead" <<<"$s"
  }
  wait_for 15 tui_markers_ready
  local scr
  scr="$(tmx capture-pane -p -t "$TUI44" 2>/dev/null)"
  assert "畫面：三條 lineage 組標頭齊全（墓碑組逐字 p4z9†，防 . 過寬匹配）" \
    bash -c 'grep -qF "lineage p4w01 (6)" <<<"$1" && grep -qF "lineage p4w07 (5)" <<<"$1" && grep -qF "lineage p4z9† (c3c3) (1)" <<<"$1"' \
    _ "$scr"
  assert "畫面：standalone 段在場且恰 3 列（rubric 條 1 後半的標的）" \
    bash -c 'grep -qF "(standalone) (3)" <<<"$1"' _ "$scr"
  # 標記面比照分組 44 的 oracle：恰一筆且綁 worker 名，多標少標兩邊都紅
  assert "畫面：⛔ 恰一列且綁定 p4w06（異常 2／3）" \
    bash -c 'test "$(grep -cF "⛔" <<<"$1")" -eq 1 && grep -F "⛔" <<<"$1" | grep -qF "p4w06"' \
    _ "$scr"
  assert "畫面：✗dead 恰一列且綁定 p4w09（異常 1／3）" \
    bash -c 'test "$(grep -cF "✗dead" <<<"$1")" -eq 1 && grep -F "✗dead" <<<"$1" | grep -qF "p4w09"' \
    _ "$scr"

  # scope 切到 unattached 看一眼再切回來（S 是 toggle，量測前必須回到 [all]）
  tmx send-keys -t "$TUI44" S
  sleep 0.8
  local scr2
  scr2="$(tmx capture-pane -p -t "$TUI44" 2>/dev/null)"
  assert "畫面：S 切到 unattached scope（標題字面）" \
    bash -c 'grep -qF "[unattached]" <<<"$1"' _ "$scr2"
  assert "畫面：unattached 恰 3 筆且含 p4ghost（rubric 條 2 後半的標的）" \
    bash -c 'grep -qF "TASKS 1/3 [unattached]" <<<"$1" && grep -qF "p4ghost" <<<"$1"' _ "$scr2"
  tmx send-keys -t "$TUI44" S
  sleep 0.8
  local scr3
  scr3="$(tmx capture-pane -p -t "$TUI44" 2>/dev/null)"
  assert "畫面：已切回 [all] scope（交給受測者的起始狀態）" \
    bash -c 'grep -qF "[all]" <<<"$1"' _ "$scr3"

  printf '\n不變式：%d PASS %d FAIL\n' "$PASS" "$FAIL"
  [[ "$FAIL" -eq 0 ]]
}

# kill-server 是非同步的：舊 server 會在自己的時間點 unlink socket。緊接著
# new-session 會撞上這個空窗——新 server 建好之後 socket 被舊的收走，之後所有
# tmx 呼叫都答「no server running」（實測踩過：第二次 up 全紅）。等它真的走完
wait_server_gone() {
  local i sockpath
  sockpath="${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SOCK"
  for (( i = 0; i < 100; i++ )); do
    if ! tmx has-session -t p5 >/dev/null 2>&1 && [[ ! -S "$sockpath" ]]; then
      return 0
    fi
    # 這版 tmux 的 kill-server 不 unlink socket：server 已死但 socket 檔殘留，
    # 上面的 -S 條件永遠不成立。「no server running」證明沒人在聽這個 socket
    # （實測：殘留 socket 下 client 回這句），自己收掉即可
    if tmx has-session -t p5 2>&1 | grep -q "no server running"; then
      rm -f "$sockpath"
      return 0
    fi
    sleep 0.1
  done
  return 1
}

do_down() {
  local rc=0 sess
  # 隔離防呆：-L 只隔離 default server，隔離不了使用者恰好同名的自訂 server。
  # socket 上若掛著不是本 fixture 的 session，kill-server 會殺掉人家的東西——拒絕
  sess="$(tmx list-sessions -F '#{session_name}' 2>/dev/null || true)"
  if [[ -n "$sess" && "$sess" != "p5" ]]; then
    printf '錯誤：socket %s 上有非本 fixture 的 session（%s），拒絕 kill-server\n' \
      "$SOCK" "${sess//$'\n'/ }" >&2
    return 2
  fi
  tmx kill-server 2>/dev/null || true
  wait_server_gone || { echo "警告：舊 tmux server 未在 10 秒內結束" >&2; rc=1; }
  if [[ -d "$STATE" ]]; then
    # 同樣的防呆：資料目錄要有本載具寫的 ownership marker 才敢遞迴刪
    if [[ -e "$STATE/.p5-fixture" ]]; then
      find "$STATE" -mindepth 1 -delete 2>/dev/null
      rmdir "$STATE" 2>/dev/null || true
    else
      echo "錯誤：$STATE 缺 ownership marker（.p5-fixture），非本載具所建，拒絕刪除" >&2
      return 2
    fi
  fi
  echo "P5 fixture 已拆除（socket $SOCK、資料 $STATE）"
  return "$rc"
}

do_up() {
  [[ -x "$BRIDGE" ]] || { echo "錯誤：找不到可執行的 $BRIDGE（先 cargo build --release && cp -f target/release/ab bin/ab）" >&2; exit 1; }
  # stdout 靜音、stderr 保留；拆不乾淨（server 沒退、非本載具資源）就拒絕 up，
  # 不在髒環境上蓋 fixture
  do_down >/dev/null || { echo "錯誤：舊環境未能乾淨拆除，拒絕 up（見上方 stderr）" >&2; exit 1; }
  mkdir -p "$D44/agents" "$D44/tasks" "$SHIM"
  : > "$STATE/.p5-fixture"

  printf '#!/usr/bin/env bash\nunset TMUX\nexec %q -L %q -f /dev/null "$@"\n' \
    "$REAL_TMUX" "$SOCK" > "$SHIM/tmux"
  chmod +x "$SHIM/tmux"
  ln -sf "$BRIDGE" "$SHIM/agent-bridge"

  local pane_cmd
  pane_cmd="$(printf 'env AGENT_BRIDGE_DATA=%q PATH=%q bash --norc --noprofile' \
    "$D44" "$SHIM:$PATH")"

  tmx new-session -d -s p5 -x 200 -y 100 "$pane_cmd"

  # owner A＝TUI 自己所在的 window（TUI 獨佔它，worker pane 另開 window，
  # 否則 TUI 被壓成十來行高，量到的變成捲動成本）
  local W44A TUI44 OWN44A
  W44A="$(tmx new-window -dP -F '#{window_id}' -t p5 "$pane_cmd")"
  TUI44="$(tmx list-panes -t "$W44A" -F '#{pane_id}' | head -1)"
  OWN44A="$(tmx display -p -t "$TUI44" '#{session_name}:#{window_id}')"
  # 早退：server 若沒起來，這些值會全是空字串，而空字串兩側相等的斷言會**通過**
  # ——fixture 於是靜默地變成一份空的 registry。這裡就要炸，不要留到量測時才發現
  [[ -n "$W44A" && -n "$TUI44" && "$OWN44A" == *:@* ]] || {
    echo "錯誤：tmux server 未就緒（W44A=$W44A TUI44=$TUI44 OWN44A=$OWN44A）" >&2
    exit 1
  }
  tmx rename-window -t "$W44A" p4a

  # owner A 的 6 個 worker pane（p4w01..p4w06）
  local W44P P4P01 P4P02 P4P03 P4P04 P4P05 P4P06 _i
  W44P="$(tmx new-window -dP -F '#{window_id}' -t p5 "$pane_cmd")"
  P4P01="$(tmx list-panes -t "$W44P" -F '#{pane_id}' | head -1)"
  for _i in 2 3 4 5 6; do
    eval "P4P0$_i=\"\$(tmx split-window -dP -F '#{pane_id}' -t \"\$W44P\" \"\$pane_cmd\")\""
  done
  tmx select-layout -t "$W44P" even-vertical
  tmx rename-window -t "$W44P" p4a-panes

  # owner B：真 window id ＋受控 session 名（owner 死活只看 @winid）
  local W44B P4P07 P4P08 P4P10 P4P11 P4P12 OWN44B
  W44B="$(tmx new-window -dP -F '#{window_id}' -t p5 "$pane_cmd")"
  tmx rename-window -t "$W44B" p4b
  P4P07="$(tmx list-panes -t "$W44B" -F '#{pane_id}' | head -1)"
  P4P08="$(tmx split-window -dP -F '#{pane_id}' -t "$W44B" "$pane_cmd")"
  P4P10="$(tmx split-window -dP -F '#{pane_id}' -t "$W44B" "$pane_cmd")"
  P4P11="$(tmx split-window -dP -F '#{pane_id}' -t "$W44B" "$pane_cmd")"
  P4P12="$(tmx split-window -dP -F '#{pane_id}' -t "$W44B" "$pane_cmd")"
  tmx select-layout -t "$W44B" even-vertical
  OWN44B="zzb:$W44B"

  # owner C：建完就 kill，window id 於是不存在（異常 3／3 的來源）
  local W44C OWN44C
  W44C="$(tmx new-window -dP -F '#{window_id}' -t p5 "$pane_cmd")"
  tmx kill-window -t "$W44C" 2>/dev/null || true
  # kill 失敗畫面就只剩 2 個異常、oracle 少鎖一個事實——這裡就要炸
  win_alive "$W44C" && { echo "錯誤：owner C 的 window $W44C 未被殺掉" >&2; exit 1; }
  OWN44C="zzc:$W44C"

  # P5 擴充列的 pane（另開 window：p4b 已滿，同 window 再切會把 pane 壓到
  # 抓不到完整畫面——分組 16a7／44 都踩過）
  local W44X P4POLD P4PMAN P4PBAD OWN44X
  W44X="$(tmx new-window -dP -F '#{window_id}' -t p5 "$pane_cmd")"
  tmx rename-window -t "$W44X" p5x
  P4POLD="$(tmx list-panes -t "$W44X" -F '#{pane_id}' | head -1)"
  P4PMAN="$(tmx split-window -dP -F '#{pane_id}' -t "$W44X" "$pane_cmd")"
  P4PBAD="$(tmx split-window -dP -F '#{pane_id}' -t "$W44X" "$pane_cmd")"
  tmx select-layout -t "$W44X" even-vertical
  OWN44X="$(tmx display -p -t "$P4POLD" '#{session_name}:#{window_id}')"

  w44() { # w44 <name> <pane> <owner> <spawn_tag> <lineage_root> [parent_agent]
    local _lin="\"lineage_root\":\"$5\""
    [[ -n "${6:-}" ]] && _lin="$_lin,\"parent_agent\":\"$6\""
    printf '{"name":"%s","pane_id":"%s","registered_at":"%s","spawned":true,"ready":true,"runtime":"codex","spawn_tag":"%s","owner":"%s",%s}\n' \
      "$1" "$2" "$REG_AT" "$4" "$3" "$_lin" > "$D44/agents/$1.json"
  }
  w44 p4w01 "$P4P01" "$OWN44A" "$P4T01" "$P4L1"
  w44 p4w02 "$P4P02" "$OWN44A" "$P4T02" "$P4L1" "$P4T01"
  w44 p4w03 "$P4P03" "$OWN44A" "$P4T03" "$P4L1" "$P4T01"
  w44 p4w04 "$P4P04" "$OWN44A" "$P4T04" "$P4L1" "$P4T01"
  w44 p4w05 "$P4P05" "$OWN44A" "$P4T05" "$P4L1" "$P4T01"
  # 異常 2／3：blocked prompt（掛在 p4w05 底下＝三節 breadcrumb）
  w44 p4w06 "$P4P06" "$OWN44A" "$P4T06" "$P4L1" "$P4T05"
  w44 p4w07 "$P4P07" "$OWN44B" "$P4T07" "$P4L2"
  w44 p4w08 "$P4P08" "$OWN44B" "$P4T08" "$P4L2" "$P4T07"
  # 異常 1／3：orphaned worker（registry 有、pane 不在，owner 仍活著）
  w44 p4w09 "%94409" "$OWN44B" "$P4T09" "$P4L2" "$P4T08"
  w44 p4w10 "$P4P10" "$OWN44B" "$P4T10" "$P4L2" "$P4T07"
  w44 p4w11 "$P4P11" "$OWN44B" "$P4T11" "$P4L2" "$P4T07"
  # 異常 3／3：origin window 已消失，且是墓碑鏈末端（root 與 parent 皆缺席）
  w44 p4w12 "$P4P12" "$OWN44C" "$P4T12" "$P4L3" "$P4L3MID"

  # ---- P5 擴充：standalone 三形狀（row_shape 的 Legacy／Manual／Invalid）----
  printf '{"name":"p4old","pane_id":"%s","registered_at":"%s","spawned":true,"ready":true,"runtime":"codex","spawn_tag":"%s","owner":"%s"}\n' \
    "$P4POLD" "$REG_AT" "$P4OLDTAG" "$OWN44X" > "$D44/agents/p4old.json"
  printf '{"name":"p4man","pane_id":"%s","registered_at":"%s","spawned":false,"ready":true,"runtime":"codex","owner":"%s"}\n' \
    "$P4PMAN" "$REG_AT" "$OWN44X" > "$D44/agents/p4man.json"
  printf '{"name":"p4bad","pane_id":"%s","registered_at":"%s","spawned":true,"ready":true,"runtime":"codex","spawn_tag":"%s","owner":"%s","lineage_root":"%s","parent_agent":""}\n' \
    "$P4PBAD" "$REG_AT" "$P4BADROOT" "$OWN44X" "$P4BADROOT" > "$D44/agents/p4bad.json"

  mktask() { # mktask <task-id> <to> <status> <created_at>
    mkdir -p "$D44/tasks/$1"
    printf '%s\n' "$3" > "$D44/tasks/$1/status"
    printf 'p4 fixture task\n' > "$D44/tasks/$1/request.md"
    printf 'p4 fixture response\n' > "$D44/tasks/$1/response.md"
    printf '{"version":1,"task_id":"%s","from":"alice","to":"%s","created_at":"%s","updated_at":"%s","working_directory":"/tmp","status":"%s"}\n' \
      "$1" "$2" "$4" "$4" "$3" > "$D44/tasks/$1/metadata.json"
  }

  # 核心 20 個 task：12 in-flight ＋ 8 終態
  local _spec _to _st _tid
  _i=0
  for _spec in \
    "p4w01 queued" "p4w01 running" "p4w02 delivered" "p4w03 running" \
    "p4w04 queued" "p4w05 running" "p4w06 running" "p4w07 queued" \
    "p4w08 delivered" "p4w09 running" "p4w10 queued" "p4w11 running" \
    "p4w01 completed" "p4w02 completed" "p4w03 failed" "p4w04 completed" \
    "p4w05 cancelled" "p4w06 completed" "p4w07 completed" "p4w12 completed"; do
    # shellcheck disable=SC2086  # 刻意 word-splitting：_spec 是「to status」兩欄
    set -- $_spec
    _to="$1"; _st="$2"
    _tid="$(printf '20260731T0000%02dZ-44%02x' "$_i" "$((160 + _i))")"
    mktask "$_tid" "$_to" "$_st" "2026-07-31T00:00:01Z"
    _i=$((_i + 1))
  done

  # 擴充：三筆連結不到的 in-flight task
  local U1="20260730T000000Z-44c0" U2="20260731T000100Z-44c1" U3="20260731T000000Z-44c2"
  mktask "$U1" p4w01  running   "2026-07-30T00:00:00Z"
  mktask "$U2" p4ghost queued   "2026-07-31T00:01:00Z"
  mktask "$U3" p4w07  delivered "$REG_AT"

  # blocked prompt 的畫面（notify::screen_has_prompt 的第一組錨）
  tmx send-keys -t "$P4P06" \
    "clear; printf 'Do you want to make this edit?\\n 1. Yes\\n 2. No, keep going\\n\\nEsc to cancel\\n'" Enter

  {
    printf 'OWN44A=%q\nOWN44B=%q\nOWN44C=%q\nOWN44X=%q\n' "$OWN44A" "$OWN44B" "$OWN44C" "$OWN44X"
    printf 'TUI44=%q\nP4P06=%q\nP4P12=%q\n' "$TUI44" "$P4P06" "$P4P12"
    printf 'P4POLD=%q\nP4PMAN=%q\nP4PBAD=%q\n' "$P4POLD" "$P4PMAN" "$P4PBAD"
    printf 'U1=%q\nU2=%q\nU3=%q\n' "$U1" "$U2" "$U3"
  } > "$ENVFILE"

  # TUI 起在 owner A 的 window
  tmx send-keys -t "$TUI44" "clear; agent-bridge ui" Enter
  tmx select-window -t "$W44A"
  sleep 1.5

  echo
  run_checks || { echo "不變式未全綠 —— 這個畫面不能拿來量 P5。" >&2; exit 1; }
  cat <<EOF

P5 量測畫面就緒。受測者請開一個新終端機（不要在既有 tmux 內），跑：

    tmux -L $SOCK attach -t p5

進去就是 TUI（window p4a），起始 scope＝[all]。量完拆掉：tests/p5-fixture.sh down
EOF
}

case "${1:-up}" in
  up)    do_up ;;
  down)  do_down ;;
  check) run_checks ;;
  *) echo "用法: $0 {up|down|check}" >&2; exit 2 ;;
esac
