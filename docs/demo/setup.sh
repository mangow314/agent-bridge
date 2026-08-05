#!/usr/bin/env bash
# README demo 錄影環境（docs/demo/*.tape 的 Hidden 階段呼叫）：
# 獨立 tmux server（-L ab-demo）＋乾淨資料目錄＋demo 用 codex stub——
# 錄影全程不打任何真實 API。可重複執行（先殺舊 server、砍舊資料目錄）。
# 用法：setup.sh [driver-script]（預設 driver.sh；各情境 tape 傳自己的編排器）
set -euo pipefail

SOCK=ab-demo
ROOT="${DEMO_ROOT:-${TMPDIR:-/tmp}/agent-bridge-demo}"
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
DRIVER="${1:-$REPO/docs/demo/driver.sh}"
[[ "$DRIVER" = /* ]] || DRIVER="$REPO/$DRIVER"

tmux -L "$SOCK" kill-server 2>/dev/null || true
pkill -f 'agent-bridge/docs/demo/.*driver' 2>/dev/null || true
rm -rf "$ROOT"
# data 子目錄預建：driver 一啟動就會掃 tasks/，不等 bridge 的 ensure_dirs
mkdir -p "$ROOT/shim" "$ROOT/data/tasks" "$ROOT/data/agents" "$ROOT/data/locks"

# 兩個 pane 各自的提示符：讓 GIF 一眼分得出誰是誰
cat > "$ROOT/orc-rc" <<'RC'
PROMPT_COMMAND=   # 系統 bashrc 的 title escape 會蓋掉 pane 標題，關掉
PS1='\[\e[1;35m\]orchestrator\[\e[0m\] ❯ '
RC
# worker/successor 共用 rc：名字從 pane title 動態取，同一個 stub 才能服務
# 不同名字的情境。bridge 設 title（「<name> (codex)」）約在 spawn 後 0.8s，
# 與 rc 執行時間點競態——短輪詢等到帶括號的 title 出現（hostname 沒有括號）
cat > "$ROOT/worker-rc" <<'RC'
PROMPT_COMMAND=
_n=""
for _ in 1 2 3 4 5 6 7 8 9 10; do
  _t="$(tmux display-message -p -t "$TMUX_PANE" '#{pane_title}' 2>/dev/null)"
  case "$_t" in *\(*\)*) _n="${_t%% *}"; break;; esac
  sleep 0.3
done
[ -n "$_n" ] || _n=worker
PS1="\[\e[1;36m\]$_n\[\e[0m\] \[\e[2m\](codex stub)\[\e[0m\] ❯ "
RC

# demo 用 codex stub：印一行誠實 banner（自報是 stand-in、報 brief 注入長度；
# spawn 注入 worker brief、relay 注入 successor brief，措辭通用），
# 啟動期吞 1 秒輸入模擬 REPL 起步（首發 ready 探針被吃、由重送覆蓋），
# 之後以 bash 當 REPL——探針與通知都會真的執行，只有「智能回覆」由
# driver 編排。形狀沿用 tests/run-tests.sh 的 runtime shim。
cat > "$ROOT/shim/codex" <<EOF
#!/usr/bin/env bash
brief="\${*: -1}"
printf '\e[2m[demo stub] codex stand-in — brief injected (%s chars); no real API calls\e[0m\n' "\${#brief}"
end=\$((SECONDS+1)); while ((SECONDS<end)); do IFS= read -r -t 0.2 _ || true; done
exec bash --rcfile "$ROOT/worker-rc"
EOF
chmod +x "$ROOT/shim/codex"

# 釘住 repo 版 agent-bridge，不受使用者 PATH 上裝了什麼影響
ln -s "$REPO/bin/agent-bridge" "$ROOT/shim/agent-bridge"

# server 環境（pane 全部繼承自它）：shim 優先、資料目錄隔離、探針間隔縮短
export PATH="$ROOT/shim:$PATH"
export AGENT_BRIDGE_DATA="$ROOT/data"
export AGENT_BRIDGE_READY_PROBE_INTERVAL=1

left="$(tmux -L "$SOCK" -f /dev/null new-session -dPF '#{pane_id}' \
  -s demo -x 200 -y 50 -c "$REPO" "bash --rcfile $ROOT/orc-rc")"

# pane 邊框顯示標題，GIF 裡角色更醒目
tmux -L "$SOCK" set -g pane-border-status top
tmux -L "$SOCK" select-pane -t "$left" -T orchestrator

# 把 orchestrator pane 註冊成 main（reply 通知才有落點）；在 pane 內執行
# 才會落在 demo server（bin 的 tmux 呼叫吃 pane 的 \$TMUX），完事清屏
tmux -L "$SOCK" send-keys -t "$left" -l "agent-bridge register main $left && clear"
sleep 0.3
tmux -L "$SOCK" send-keys -t "$left" Enter

# worker 側編排器：等任務送達後演出對應情境的 start → reply
setsid "$DRIVER" "$ROOT/data" >"$ROOT/driver.log" 2>&1 < /dev/null &

echo "demo env ready: socket=$SOCK root=$ROOT left=$left" >&2
