#!/usr/bin/env bash
# check-contract.sh [1 2 3 4]：spec/ 與實作／測試套件的形狀交叉核對。
# 無參數＝跑全部。grep/awk 級集合比對，不解析語意（語意由測試套件把關）。
#   1  env：實作正本內 AGENT_BRIDGE_* 變數集合 == spec/env.md 條款集合
#   2  cli：實作宣稱的子指令每個在 spec/cli.md 有對應章節
#   3  hooks：hook_* 函式與三事件在 spec/hooks.md 有條款
#   4  traceability：run-tests.sh 每個編號分組被 traceability.md 引用 >=1 次；
#      統計 [untested] 數並比對 traceability.md 頂部宣告
#   5  scope：spec/cli.md 每個子指令在 docs/scope-2026-09.md 的分類表恰出現
#      一次，且分類 ∈ keep|support|freeze
#
# 實作正本是 Rust（bash 正本已退役）：
#   1  grep crates/**/*.rs
#   2  `ab __implemented-commands`（隱藏內省指令，非 spec 條款面）——比 grep
#      dispatch 表抗重構，且 M1–M3 的里程碑 gate 已在用同一支
set -u
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

SRC_RUST=crates
# 內省指令的載具：預設走 shim（＝套件的 BRIDGE 預設），可用 BRIDGE 指別的執行檔
BRIDGE="${BRIDGE:-$PWD/bin/agent-bridge}"
SPEC=spec
TESTS=tests/run-tests.sh
FAIL=0

# 載具身分：判別式用既有的 `__implemented-commands`（Rust 正本認得，rc 0），
# 不必為了自報身分再開一個介面。$BRIDGE 可由呼叫者覆寫指到任何執行檔，這道
# 因此仍有意義——指錯載具會讓集合比對驗錯對象。
# 探針的 DATA 指到不可建立的路徑：不能讓誤走的分支動到使用者真實的
# ~/.local/share/agent-bridge（失敗照樣是非零，判別結果不變）。
bridge_is_rust() {
  AGENT_BRIDGE_DATA=/dev/null/probe "$BRIDGE" __implemented-commands \
    >/dev/null 2>&1
}

fail() { printf 'FAIL %s\n' "$1"; FAIL=1; }
ok()   { printf 'ok   %s\n' "$1"; }

check_1() {
  local want got
  # [A-Z] 起頭強制至少一字元：env.md 行文中的通配寫法 `AGENT_BRIDGE_*` 不算名稱
  want="$(grep -rhoE 'AGENT_BRIDGE_[A-Z][A-Z_]*' --include='*.rs' "$SRC_RUST" | sort -u)"
  got="$(grep -oE 'AGENT_BRIDGE_[A-Z][A-Z_]*' "$SPEC/env.md" | sort -u)"
  if [[ "$want" == "$got" ]]; then
    ok "1 env 集合一致（$(wc -l <<<"$want") 個）"
  else
    fail "1 env 集合不一致："
    diff <(printf '%s\n' "$want") <(printf '%s\n' "$got") | sed 's/^/     /'
  fi
}

check_2() {
  local cmds spec_cmds n
  # 載具必須真的是 Rust 正本：指錯執行檔會驗錯對象
  if ! bridge_is_rust; then
    fail "2 載具不是 Rust 正本（不認得 __implemented-commands）：$BRIDGE"
    return
  fi
  # 執行檔宣稱的實作集合。空輸出＝載具壞了，別讓集合比對以「兩邊都空」收場
  cmds="$("$BRIDGE" __implemented-commands 2>/dev/null | sort -u)" || cmds=""
  if [[ -z "$cmds" ]]; then
    fail "2 取不到 __implemented-commands（載具：$BRIDGE）"
    return
  fi
  # **雙向**集合相等，不是單向包含：單向只驗「宣稱的都在 spec」，一個退化成
  # 只印一個命令的載具照樣印 ok（獨立複核 2026-07-31 的 mutation 實證）。
  # 反向那半（spec 有章節、實作沒有）本來就是 cutover 後最該紅的形狀
  # shellcheck disable=SC2016  # 單引號是刻意的：pattern 裡的反引號是字面值
  spec_cmds="$(grep -o '^## `[a-z-]*`$' "$SPEC/cli.md" | tr -d '#` ' | sort -u)"
  if [[ "$cmds" != "$spec_cmds" ]]; then
    fail "2 子指令集合與 cli.md 章節不一致（載具：$BRIDGE）："
    diff <(printf '%s\n' "$cmds") <(printf '%s\n' "$spec_cmds") | sed 's/^/     /'
    return
  fi
  n="$(grep -c . <<<"$cmds")"
  ok "2 子指令集合與 cli.md 章節完全一致（$n 個）"
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

check_5() {
  local scope=docs/scope-2026-09.md spec_cmds section cand rows bad dup missing
  # 只看「## 逐命令分類」一節；節內每一條以「| `」起頭的列都是候選資料列，
  # 解析不出嚴格形狀 | `cmd` | keep|support|freeze | … | 就紅（不能靜默漏列——
  # 複核 mutation 實證：多一格空白的重複列曾被寬鬆 grep 直接略過而仍綠）
  section="$(awk '/^## 逐命令分類/{f=1;next} /^## /{f=0} f' "$scope" 2>/dev/null)"
  # shellcheck disable=SC2016  # 反引號是字面值
  cand="$(grep '^| `' <<<"$section")"
  if [[ -z "$cand" ]]; then
    fail "5 $scope 缺「逐命令分類」表"
    return
  fi
  # shellcheck disable=SC2016
  bad="$(grep -vE '^\| `[a-z-]+` \| (keep|support|freeze) \| ' <<<"$cand")"
  if [[ -n "$bad" ]]; then
    fail "5 scope 分類表有無法解析的列："
    sed 's/^/     /' <<<"$bad"
    return
  fi
  # shellcheck disable=SC2016
  rows="$(sed -E 's/^\| `([a-z-]+)` \| ([a-z]+) \| .*$/\1 \2/' <<<"$cand")"
  dup="$(awk '{print $1}' <<<"$rows" | sort | uniq -d)"
  # shellcheck disable=SC2016
  spec_cmds="$(grep -o '^## `[a-z-]*`$' "$SPEC/cli.md" | tr -d '#` ' | sort -u)"
  missing="$(diff <(printf '%s\n' "$spec_cmds") <(awk '{print $1}' <<<"$rows" | sort -u))"
  if [[ -n "$dup" || -n "$missing" ]]; then
    fail "5 scope 分類表與 cli.md 不一致："
    [[ -n "$dup" ]] && printf '     重複列：%s\n' "$dup"
    [[ -n "$missing" ]] && sed 's/^/     /' <<<"$missing"
    return
  fi
  ok "5 scope 分類表涵蓋 cli.md 全部子指令、各恰一次（$(grep -c . <<<"$rows") 個）"
}

if (( $# == 0 )); then set -- 1 2 3 4 5; fi
for item in "$@"; do
  case "$item" in
    1) check_1 ;; 2) check_2 ;; 3) check_3 ;; 4) check_4 ;; 5) check_5 ;;
    *) fail "未知項目：$item" ;;
  esac
done
exit "$FAIL"
