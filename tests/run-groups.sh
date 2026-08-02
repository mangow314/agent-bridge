#!/usr/bin/env bash
# tests/run-groups.sh — agent-bridge 測試分組執行載具（薄委派）
#
# 用法：tests/run-groups.sh <groups>
#       <groups> 為逗號分隔之分組編號清單（例如 1,2,8a,34.5+）或 all。
#
# 2026-08-03 改寫：早期版本用 awk 從 run-tests.sh 抽段拼暫存腳本執行——
# 那會繞過 run-tests.sh 內建的 TEST_GROUPS 機制（跨組依賴閉包 GRP_NEEDS、
# `⚠ PARTIAL RUN` 證據標記，見 docs/testing-policy.md），產生沒有 partial
# 標記、也沒拉依賴組的輸出（codex 複核 blocking finding）。現在一律委派
# 完整的 run-tests.sh：分組選擇、依賴閉包、partial 標記單一來源。
set -u

if [[ $# -ne 1 || -z "$1" ]]; then
  echo "用法: $0 <groups|all>" >&2
  echo "範例: $0 1,2,8a,34.5+" >&2
  echo "      $0 all" >&2
  exit 2
fi

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN_TESTS="$TESTS_DIR/run-tests.sh"
if [[ ! -f "$RUN_TESTS" ]]; then
  echo "錯誤：找不到測試主腳本 $RUN_TESTS" >&2
  exit 1
fi

if [[ "$1" == "all" ]]; then
  exec "$RUN_TESTS"          # all＝全套，不設 TEST_GROUPS（收案證據形）
fi
TEST_GROUPS="$1" exec "$RUN_TESTS"
