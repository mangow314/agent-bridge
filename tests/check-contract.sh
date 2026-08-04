#!/usr/bin/env bash
# check-contract.sh [1 2 3 4]：spec/ 與實作／測試套件的形狀交叉核對。
# 無參數＝跑全部。grep/awk 級集合比對，不解析語意（語意由測試套件把關）。
#   1  env：實作正本內 AGENT_BRIDGE_* 變數集合 == spec/env.md 條款集合
#   2  cli：實作宣稱的子指令每個在 spec/cli.md 有對應章節
#   3  hooks：hook_* 函式與三事件在 spec/hooks.md 有條款
#   4  traceability：run-tests.sh 每個編號分組被 traceability.md 引用 >=1 次；
#      統計 [untested] 數並比對 traceability.md 頂部宣告
#
# M4 cutover：1／2 的來源從 bash 正本改綁 Rust（正本已是 Rust）。
# SRC_KIND 顯式可覆蓋——`SRC_KIND=bash tests/check-contract.sh 1 2` 仍可對
# bin/agent-bridge.bash 核對，rollback 期與雙實作對照都用得上。
#   1  rust: grep crates/**/*.rs（與 bash 的 grep 同構）
#   2  rust: `ab __implemented-commands`（隱藏內省指令，非 spec 條款面）——
#      比 grep dispatch 表抗重構，且 M1–M3 的里程碑 gate 已在用同一支
set -u
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

SRC_KIND="${SRC_KIND:-rust}"
case "$SRC_KIND" in
  rust|bash) ;;
  # 未知值不得靜默落進某個分支：拼錯的 SRC_KIND 會讓人以為驗了 A 其實驗了 B
  *) printf 'SRC_KIND 需為 rust 或 bash：%s\n' "$SRC_KIND" >&2; exit 2 ;;
esac
BIN_BASH=bin/agent-bridge.bash
SRC_RUST=crates
# 內省指令的載具：預設走 shim（＝套件的 BRIDGE 預設），可用 BRIDGE 指別的執行檔
BRIDGE="${BRIDGE:-$PWD/bin/agent-bridge}"
SPEC=spec
TESTS=tests/run-tests.sh
FAIL=0

# 載具是哪套實作。判別式用既有的 `__implemented-commands`：Rust 認得（rc 0）、
# bash 正本當成未知指令（rc 1），不必為了自報身分再開一個介面。
# 探針的 DATA 指到不可建立的路徑：bash 分支若走到建目錄，不能讓它動到
# 使用者真實的 ~/.local/share/agent-bridge（失敗照樣是非零，判別結果不變）。
bridge_kind() {
  if AGENT_BRIDGE_DATA=/dev/null/probe "$BRIDGE" __implemented-commands \
      >/dev/null 2>&1; then
    printf 'rust\n'
  else
    printf 'bash\n'
  fi
}

fail() { printf 'FAIL %s\n' "$1"; FAIL=1; }
ok()   { printf 'ok   %s\n' "$1"; }

check_1() {
  local want got
  # [A-Z] 起頭強制至少一字元：env.md 行文中的通配寫法 `AGENT_BRIDGE_*` 不算名稱
  if [[ "$SRC_KIND" == bash ]]; then
    want="$(grep -oE 'AGENT_BRIDGE_[A-Z][A-Z_]*' "$BIN_BASH" | sort -u)"
  else
    want="$(grep -rhoE 'AGENT_BRIDGE_[A-Z][A-Z_]*' --include='*.rs' "$SRC_RUST" | sort -u)"
  fi
  got="$(grep -oE 'AGENT_BRIDGE_[A-Z][A-Z_]*' "$SPEC/env.md" | sort -u)"
  if [[ "$want" == "$got" ]]; then
    ok "1 env 集合一致（$(wc -l <<<"$want") 個）"
  else
    fail "1 env 集合不一致："
    diff <(printf '%s\n' "$want") <(printf '%s\n' "$got") | sed 's/^/     /'
  fi
}

check_2() {
  local cmds spec_cmds kind n
  if [[ "$SRC_KIND" == bash ]]; then
    cmds="$(grep -o '^cmd_[a-z_]*()' "$BIN_BASH" | sed 's/^cmd_//; s/()$//' | sort -u)"
  else
    # 載具必須真的是 Rust：SRC_KIND=rust 配上 bash 正本會驗錯對象
    kind="$(bridge_kind)"
    if [[ "$kind" != rust ]]; then
      fail "2 SRC_KIND=rust 但載具偵測為 $kind：$BRIDGE"
      return
    fi
    # 執行檔宣稱的實作集合。空輸出＝載具壞了，別讓集合比對以「兩邊都空」收場
    cmds="$("$BRIDGE" __implemented-commands 2>/dev/null | sort -u)" || cmds=""
    if [[ -z "$cmds" ]]; then
      fail "2 取不到 __implemented-commands（載具：$BRIDGE）"
      return
    fi
  fi
  # **雙向**集合相等，不是單向包含：單向只驗「宣稱的都在 spec」，一個退化成
  # 只印一個命令的載具照樣印 ok（獨立複核 2026-07-31 的 mutation 實證）。
  # 反向那半（spec 有章節、實作沒有）本來就是 cutover 後最該紅的形狀
  # shellcheck disable=SC2016  # 單引號是刻意的：pattern 裡的反引號是字面值
  spec_cmds="$(grep -o '^## `[a-z-]*`$' "$SPEC/cli.md" | tr -d '#` ' | sort -u)"
  if [[ "$cmds" != "$spec_cmds" ]]; then
    fail "2 子指令集合與 cli.md 章節不一致（源：$SRC_KIND）："
    diff <(printf '%s\n' "$cmds") <(printf '%s\n' "$spec_cmds") | sed 's/^/     /'
    return
  fi
  n="$(grep -c . <<<"$cmds")"
  ok "2 子指令集合與 cli.md 章節完全一致（$n 個，源：$SRC_KIND）"
}

# check 3 不受 cutover 影響：名單是硬編的，比對對象只有 spec/hooks.md，
# 從來沒讀過實作正本。hook_* 這四個名字在 Rust 側續存於 ab-core/src/hook.rs
# 的對映註解（每個函式標了它對應的 bash 函式與行號），spec 的 Source 標記
# 因此仍指得到實作
check_3() {
  local missing=0 h
  for h in hook_agent_name hook_write_state hook_owner_gate hook_oldest_queued; do
    grep -q "$h" "$SPEC/hooks.md" 2>/dev/null || { fail "3 hooks.md 未提及函式：$h"; missing=1; }
  done
  for h in stop prompt-submit notification; do
    grep -q -- "$h" "$SPEC/hooks.md" 2>/dev/null || { fail "3 hooks.md 未提及事件：$h"; missing=1; }
  done
  (( missing )) || ok "3 hooks.md 涵蓋 4 函式＋3 事件"
}

check_4() {
  local missing=0 n untested declared
  # 編號分組：run-tests.sh 的 `# ---- N.` 標頭（N 可含小數與字母尾碼，如 8a、34.5+）
  # -F literal 比對整個表格 cell（含兩側 |）：分組名含 regex metacharacter
  # （34.5+ 的 +）時 -E 會退化成量詞，錯誤編號 34.55 可蒙混（複核 mutation 實證）
  while IFS= read -r n; do
    grep -qF "| $n |" "$SPEC/traceability.md" 2>/dev/null \
      || { fail "4 traceability.md 未引用分組：$n"; missing=1; }
  done < <(grep -o '^# ---- [0-9][0-9.a-z+]*' "$TESTS" | awk '{print $3}' | sed 's/\.$//')
  # 只數條款標題級的 [untested]（行文提及不算）
  untested="$(grep -hcE '^### [A-Z][A-Z-]*-[0-9]+ \[untested\]' "$SPEC"/*.md 2>/dev/null | awk '{s+=$1} END{print s}')"
  declared="$(grep -o 'untested 計數：[0-9]*' "$SPEC/traceability.md" 2>/dev/null | grep -o '[0-9]*$')"
  if [[ -z "$declared" ]]; then
    fail "4 traceability.md 缺 untested 計數宣告"
  elif [[ "$untested" != "$declared" ]]; then
    fail "4 untested 計數不符：spec 實有 $untested、宣告 $declared"
  elif (( ! missing )); then
    ok "4 分組引用完整；untested 計數一致（$untested）"
  fi
}

if (( $# == 0 )); then set -- 1 2 3 4; fi
for item in "$@"; do
  case "$item" in
    1) check_1 ;; 2) check_2 ;; 3) check_3 ;; 4) check_4 ;;
    *) fail "未知項目：$item" ;;
  esac
done
exit "$FAIL"
