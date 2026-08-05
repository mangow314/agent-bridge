# 情境編排器共用函式（被各 *-driver.sh source；呼叫端先設好 DATA）。
# 形狀沿用 driver.sh：等檔案系統事件、往 worker pane 敲劇本台詞。

SOCK=ab-demo

tmx() { tmux -L "$SOCK" "$@"; }
# 仿 bin 的通知形狀：文字與 Enter 拆兩次送，避免被 REPL 當貼上吞掉
send() { tmx send-keys -t "$pane" -l "$1"; sleep 0.25; tmx send-keys -t "$pane" Enter; }

pane_of() { jq -r '.pane_id' "$DATA/agents/$1.json"; }

# 等 tasks/ 出現第 n 個任務並印其 id（task-id 帶 timestamp，字典序＝時間序）
wait_task_n() {
  local n="$1" id=""
  for _ in $(seq 1 240); do
    # tasks/ 在首次 agent-bridge 呼叫前不存在，pipefail 下整條兜底
    id="$(find "$DATA/tasks" -mindepth 1 -maxdepth 1 -printf '%f\n' 2>/dev/null | sort | sed -n "${n}p" || true)"
    [[ -n "$id" ]] && { printf '%s' "$id"; return 0; }
    sleep 0.5
  done
  echo "driver: 等不到第 $n 個任務" >&2
  return 1
}

# 等指定 task 翻到指定狀態（worker pane 的 receive 跑完＝delivered）
wait_status() {
  for _ in $(seq 1 240); do
    [[ "$(cat "$DATA/tasks/$1/status" 2>/dev/null)" == "$2" ]] && return 0
    sleep 0.5
  done
  echo "driver: task $1 等不到狀態 $2" >&2
  return 1
}

# 等 agent 註冊檔出現（relay／spawn 完成的檔案系統證據）
wait_agent() {
  for _ in $(seq 1 240); do
    [[ -f "$DATA/agents/$1.json" ]] && return 0
    sleep 0.5
  done
  echo "driver: 等不到 agent $1 註冊" >&2
  return 1
}
