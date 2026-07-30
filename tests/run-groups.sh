#!/usr/bin/env bash
# tests/run-groups.sh — agent-bridge 測試分組執行載具
#
# 用法：tests/run-groups.sh <groups>
#       <groups> 為逗號分隔之分組編號清單（例如 1,2,8a,34.5+）或 all。
#
# 原理：
# - 從同目錄 run-tests.sh 抽取序幕 (prologue)、指定分組 (groups) 與總結段 (summary)
# - 拼接至 tests/ 目錄下之暫存腳本執行，確保同目錄位置不破壞 ROOT 推導
# - 不修改 run-tests.sh 本體，並確保總結段必附以防假綠判定
set -u

# 1. 驗證命令列參數
if [[ $# -ne 1 || -z "$1" ]]; then
  echo "用法: $0 <groups|all>" >&2
  echo "範例: $0 1,2,8a,34.5+" >&2
  echo "      $0 all" >&2
  exit 2
fi

GROUPS_ARG="$1"

# 2. 推導專案根目錄與周邊路徑
# 依據本腳本（tests/run-groups.sh）位置推算 repository 根目錄
TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$TESTS_DIR/.." && pwd)"
RUN_TESTS="$TESTS_DIR/run-tests.sh"

if [[ ! -f "$RUN_TESTS" ]]; then
  echo "錯誤：找不到測試主腳本 $RUN_TESTS" >&2
  exit 1
fi

# 顯式匯出 BRIDGE 變數，若呼叫端未指定則預設指向 repo 根目錄之 bin/agent-bridge
export BRIDGE="${BRIDGE:-$ROOT/bin/agent-bridge}"

# 3. 建立暫存腳本於 tests/ 目錄下
# 關鍵：prologue 內部 `ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"`
# 依據暫存腳本檔名推算 repo 根目錄。若建立於 /tmp，路徑推導將會破壞。
TMPFILE="$(mktemp "$TESTS_DIR/.run-groups.XXXXXX.sh")"

# 註冊 EXIT 觸發清理函式，確保無論正常結束或中途異常皆能刪除暫存檔
# shellcheck disable=SC2329  # 經 trap EXIT 間接呼叫
cleanup() {
  if [[ -n "${TMPFILE:-}" && -f "$TMPFILE" ]]; then
    rm -f "$TMPFILE"
  fi
}
trap cleanup EXIT

# 4. 使用 awk 解析 run-tests.sh 並拼接暫存腳本
# - 編號標頭正則 `^# ---- [0-9][0-9.a-z+]*`，取第 3 欄並去尾端句點（與 check-contract.sh 一致）
# - 序幕 (prologue)：第一個編號標頭前的所有內容
# - 分組段 (group)：標頭至下一個編號標頭或總結段前
# - 總結段 (summary)：檔尾 `# ---- 總結` 起至 EOF，任何選組皆必附
awk -v req="$GROUPS_ARG" '
BEGIN {
  # 拆解請求的分組清單
  nreq = split(req, r_arr, ",")
  for (i = 1; i <= nreq; i++) {
    gsub(/^[ \t]+|[ \t]+$/, "", r_arr[i])
    if (r_arr[i] != "") {
      if (r_arr[i] == "all") {
        is_all = 1
      } else {
        req_map[r_arr[i]] = 1
      }
    }
  }

  state = "prologue"
  group_cnt = 0
}

{
  if (state == "prologue") {
    if (/^# ---- [0-9][0-9.a-z+]*/) {
      state = "group"
      gid = $3
      sub(/\.$/, "", gid)
      group_cnt++
      group_order[group_cnt] = gid
      valid_map[gid] = 1
      group_buf[gid] = $0 "\n"
    } else {
      prologue = prologue $0 "\n"
    }
  } else if (state == "group") {
    if (/^# ---- 總結/) {
      state = "summary"
      summary = $0 "\n"
    } else if (/^# ---- [0-9][0-9.a-z+]*/) {
      gid = $3
      sub(/\.$/, "", gid)
      group_cnt++
      group_order[group_cnt] = gid
      valid_map[gid] = 1
      group_buf[gid] = $0 "\n"
    } else {
      group_buf[gid] = group_buf[gid] $0 "\n"
    }
  } else if (state == "summary") {
    summary = summary $0 "\n"
  }
}

END {
  # 檢查是否存在未知的分組 ID
  invalid_cnt = 0
  for (i = 1; i <= nreq; i++) {
    item = r_arr[i]
    if (item != "" && item != "all" && !(item in valid_map)) {
      invalid_cnt++
      invalid_list = (invalid_cnt == 1 ? item : invalid_list ", " item)
    }
  }

  if (invalid_cnt > 0) {
    valid_list = ""
    for (i = 1; i <= group_cnt; i++) {
      valid_list = (i == 1 ? group_order[i] : valid_list ", " group_order[i])
    }
    printf "錯誤：未知分組 id [%s]\n", invalid_list > "/dev/stderr"
    printf "合法分組 id 清單：%s\n", valid_list > "/dev/stderr"
    exit 2
  }

  # 輸出序幕
  printf "%s", prologue

  # 依據原檔出現順序輸出選定分組，與參數指定順序無關
  for (i = 1; i <= group_cnt; i++) {
    gid = group_order[i]
    if (is_all || (gid in req_map)) {
      printf "%s", group_buf[gid]
    }
  }

  # 輸出總結段
  printf "%s", summary
}
' "$RUN_TESTS" > "$TMPFILE"

awk_rc=$?
if (( awk_rc != 0 )); then
  exit "$awk_rc"
fi

# 5. 以 bash 執行暫存腳本並原樣傳遞退出碼
bash "$TMPFILE"
rc=$?
exit "$rc"
